/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A Job is an abstraction of async operation in service worker lifecycle propagation.
//! Each Job is uniquely identified by its scope_url, and is keyed accordingly under
//! the script thread. The script thread contains a JobQueue, which stores all scheduled Jobs
//! by multiple service worker clients in a Vec.

use dom::bindings::cell::DOMRefCell;
use dom::bindings::error::Error;
use dom::bindings::js::JS;
use dom::bindings::refcounted::Trusted;
use dom::bindings::reflector::DomObject;
use dom::client::Client;
use dom::globalscope::GlobalScope;
use dom::promise::Promise;
use dom::serviceworkerregistration::ServiceWorkerRegistration;
use dom::urlhelper::UrlHelper;
use dom::response::Response;
use hyper::header::ContentType;
use mime::{Mime, TopLevel, SubLevel};
use net_traits::response::HttpsState;
use net_traits::request::{Request, CacheMode, RedirectMode};
use script_thread::{ScriptThread, Runnable};
use script_runtime::{CommonScriptMsg, ScriptThreadEventCategory};
use servo_url::ServoUrl;
use std::cmp::PartialEq;
use std::collections::HashMap;
use std::rc::Rc;
use msg::constellation_msg::PipelineId;
use network_listener::{NetworkListener, PreInvoke};
use dom::bindings::refcounted::TrustedPromise;
use net_traits::{FilteredMetadata, FetchMetadata, Metadata};
use net_traits::{FetchResponseListener, NetworkError};
use ipc_channel::ipc;
use ipc_channel::router::ROUTER;
use std::sync::{Arc, Mutex};
use url::Url;
use net_traits::CoreResourceMsg::Fetch as WorkerScriptFetchMsg;
use fetch::request_init_from_request;

#[derive(PartialEq, Clone, Debug, JSTraceable)]
pub enum JobType {
    Register,
    Unregister,
    Update
}

#[derive(Clone)]
pub enum SettleType {
    Resolve(Trusted<ServiceWorkerRegistration>),
    Reject(Error)
}

// This encapsulates what operation to invoke of JobQueue from script thread
pub enum InvokeType {
    Settle(SettleType),
    Run,
    Register,
    Update
}

#[must_root]
#[derive(JSTraceable)]
pub struct Job {
    pub job_type: JobType,
    pub scope_url: ServoUrl,
    pub script_url: ServoUrl,
    pub promise: Rc<Promise>,
    pub equivalent_jobs: Vec<Job>,
    // client can be a window client, worker client so `Client` will be an enum in future
    pub client: JS<Client>,
    pub referrer: ServoUrl,
    pub force_bypass_cache: bool
}

impl Job {
    #[allow(unrooted_must_root)]
    // https://w3c.github.io/ServiceWorker/#create-job-algorithm
    pub fn create_job(job_type: JobType,
                      scope_url: ServoUrl,
                      script_url: ServoUrl,
                      promise: Rc<Promise>,
                      client: &Client) -> Job {
        Job {
            job_type: job_type,
            scope_url: scope_url,
            script_url: script_url,
            promise: promise,
            equivalent_jobs: vec![],
            client: JS::from_ref(client),
            referrer: client.creation_url(),
            force_bypass_cache: false
        }
    }
    #[allow(unrooted_must_root)]
    pub fn append_equivalent_job(&mut self, job: Job) {
        self.equivalent_jobs.push(job);
    }
}

impl PartialEq for Job {
    // Equality criteria as described in https://w3c.github.io/ServiceWorker/#dfn-job-equivalent
    fn eq(&self, other: &Self) -> bool {
        let same_job = self.job_type == other.job_type;
        if same_job {
            match self.job_type {
                JobType::Register | JobType::Update => {
                    self.scope_url == other.scope_url && self.script_url == other.script_url
                },
                JobType::Unregister => self.scope_url == other.scope_url
            }
        } else {
            false
        }
    }
}

pub struct FinishJobHandler {
    pub scope_url: ServoUrl,
    pub global: Trusted<GlobalScope>,
}

impl FinishJobHandler {
    pub fn new(scope_url: ServoUrl, global: Trusted<GlobalScope>) -> FinishJobHandler {
        FinishJobHandler {
            scope_url: scope_url,
            global: global
        }
    }
}

impl Runnable for FinishJobHandler {
    fn main_thread_handler(self: Box<FinishJobHandler>, script_thread: &ScriptThread) {
        script_thread.invoke_finish_job(self);
    }
}

pub struct AsyncJobHandler {
    pub scope_url: ServoUrl,
    pub invoke_type: InvokeType
}

impl AsyncJobHandler {
    fn new(scope_url: ServoUrl, invoke_type: InvokeType) -> AsyncJobHandler {
        AsyncJobHandler {
            scope_url: scope_url,
            invoke_type: invoke_type
        }
    }
}

impl Runnable for AsyncJobHandler {
    #[allow(unrooted_must_root)]
    fn main_thread_handler(self: Box<AsyncJobHandler>, script_thread: &ScriptThread) {
        script_thread.dispatch_job_queue(self);
    }
}

#[must_root]
#[derive(JSTraceable)]
pub struct JobQueue(pub DOMRefCell<HashMap<ServoUrl, Vec<Job>>>);

impl JobQueue {
    pub fn new() -> JobQueue {
        JobQueue(DOMRefCell::new(HashMap::new()))
    }
    #[allow(unrooted_must_root)]
    // https://w3c.github.io/ServiceWorker/#schedule-job-algorithm
    pub fn schedule_job(&self,
                        job: Job,
                        global: &GlobalScope,
                        script_thread: &ScriptThread) {
        let mut queue_ref = self.0.borrow_mut();
        let job_queue = queue_ref.entry(job.scope_url.clone()).or_insert(vec![]);
        // Step 1
        if job_queue.is_empty() {
            let scope_url = job.scope_url.clone();
            job_queue.push(job);
            let run_job_handler = AsyncJobHandler::new(scope_url, InvokeType::Run);
            script_thread.queue_serviceworker_job(box run_job_handler, global);
        } else {
            // Step 2
            let mut last_job = job_queue.pop().unwrap();
            if job == last_job && !last_job.promise.is_settled() {
                last_job.append_equivalent_job(job);
                job_queue.push(last_job);
            } else {
                // restore the popped last_job
                job_queue.push(last_job);
                // and push this new job to job queue
                job_queue.push(job);
            }
        }
    }

    #[allow(unrooted_must_root)]
    // https://w3c.github.io/ServiceWorker/#run-job-algorithm
    pub fn run_job(&self, run_job_handler: Box<AsyncJobHandler>, script_thread: &ScriptThread) {
        let queue_ref = &*self.0.borrow();
        let front_job =  {
            let job_vec = queue_ref.get(&run_job_handler.scope_url);
            job_vec.unwrap().first().unwrap()
        };
        let global = &*front_job.client.global();
        let handler = *run_job_handler;
        match front_job.job_type {
            JobType::Register => {
                let register_job_handler = AsyncJobHandler::new(handler.scope_url, InvokeType::Register);
                script_thread.queue_serviceworker_job(box register_job_handler, global);
            },
            JobType::Update => {
                let update_job_handler = AsyncJobHandler::new(handler.scope_url, InvokeType::Update);
                script_thread.queue_serviceworker_job(box update_job_handler, global);
            }
            _ => { /* TODO implement Unregister */ }
        }
    }

    #[allow(unrooted_must_root)]
    // https://w3c.github.io/ServiceWorker/#register-algorithm
    pub fn run_register(&self, job: &Job, register_job_handler: Box<AsyncJobHandler>, script_thread: &ScriptThread) {
        let global = &*job.client.global();
        let AsyncJobHandler { scope_url, .. } = *register_job_handler;
        // Step 1-3
        if !UrlHelper::is_origin_trustworthy(&job.script_url) {
            let settle_type = SettleType::Reject(Error::Type("Invalid script ServoURL".to_owned()));
            let async_job_handler = AsyncJobHandler::new(scope_url, InvokeType::Settle(settle_type));
            return script_thread.queue_serviceworker_job(box async_job_handler, global);
        } else if job.script_url.origin() != job.referrer.origin() || job.scope_url.origin() != job.referrer.origin() {
            let settle_type = SettleType::Reject(Error::Security);
            let async_job_handler = AsyncJobHandler::new(scope_url, InvokeType::Settle(settle_type));
            return script_thread.queue_serviceworker_job(box async_job_handler, global);
        }
        // Step 4-5
        if let Some(reg) = script_thread.handle_get_registration(&job.scope_url) {
            // Step 5.1
            if reg.get_uninstalling() {
                reg.set_uninstalling(false);
            }
            // Step 5.3
            if let Some(ref newest_worker) = reg.get_newest_worker() {
                if (&*newest_worker).get_script_url() == job.script_url {
                    let settle_type = SettleType::Resolve(Trusted::new(&*reg));
                    let async_job_handler = AsyncJobHandler::new(scope_url, InvokeType::Settle(settle_type));
                    script_thread.queue_serviceworker_job(box async_job_handler, global);
                    let finish_job_handler = box FinishJobHandler::new(job.scope_url.clone(), Trusted::new(&*global));
                    script_thread.queue_finish_job(finish_job_handler, &*global);
                }
            }
        } else {
            // Step 6.1
            let pipeline = global.pipeline_id();
            let new_reg = ServiceWorkerRegistration::new(&*global, &job.script_url, scope_url);
            script_thread.handle_serviceworker_registration(&job.scope_url, &*new_reg, pipeline);
        }
        // Step 7
        script_thread.invoke_job_update(job, &*global);
    }

    #[allow(unrooted_must_root)]
    // https://w3c.github.io/ServiceWorker/#finish-job-algorithm
    pub fn finish_job(&self, scope_url: ServoUrl, global: &GlobalScope, script_thread: &ScriptThread) {
        if let Some(job_vec) = (*self.0.borrow_mut()).get_mut(&scope_url) {
            if job_vec.first().map_or(false, |job| job.scope_url == scope_url) {
                let _  = job_vec.remove(0);
            }
            if !job_vec.is_empty() {
                let run_job_handler = AsyncJobHandler::new(scope_url, InvokeType::Run);
                script_thread.queue_serviceworker_job(box run_job_handler, global);
            }
        } else {
            warn!("non-existent job vector for Servourl: {:?}", scope_url);
        }
    }

    // https://w3c.github.io/ServiceWorker/#update-algorithm
    pub fn update(&self, job: &Job, global: &GlobalScope, script_thread: &ScriptThread) {
        let reg = match script_thread.handle_get_registration(&job.scope_url) {
            Some(reg) => reg,
            None => return
        };
        // Step 1
        if reg.get_uninstalling() {
            return reject_promise_job("Update called on an uninstalling registration",
                                      script_thread,
                                      global,
                                      &job.scope_url);
        }
        let newest_worker = match reg.get_newest_worker() {
            Some(worker) => worker,
            None => return
        };
        // Step 2
        if (&*newest_worker).get_script_url() == job.script_url  && job.job_type == JobType::Update {
            // Step 4
            reject_promise_job("Invalid script ServoURL", script_thread, global, &job.scope_url);
        } else {
            // Step 5
            let mut https_state: Option<HttpsState> = None;
            let mut referrer_policy = String::new();
            let pipeline = global.pipeline_id();
            // We fetch a classic worker script by default as we dont support modules now.
            let mut req = Request::get_classic_fetch_req(job.script_url.clone(), pipeline);
            // https://html.spec.whatwg.org/multipage/webappapis.html#fetching-scripts-perform-fetch
            // `is top level` flag is true for all classic script fetches
            perform_fetch_hook_and_fetch(req,
                                         true,
                                         reg.should_use_cache(),
                                         job.force_bypass_cache,
                                         &*reg,
                                         global, 
                                         &job.scope_url);
            println!("Else part running");

            // Ok so perform_fetch_hook_and_fetch after retrieving the script can then call
            // job queue to perform

            /*job.client.set_controller(&*newest_worker);
            let settle_type = SettleType::Resolve(Trusted::new(&*reg));
            let async_job_handler = AsyncJobHandler::new(job.scope_url.clone(), InvokeType::Settle(settle_type));
            script_thread.queue_serviceworker_job(box async_job_handler, global);*/
        }
        /*let finish_job_handler = box FinishJobHandler::new(job.scope_url.clone(), Trusted::new(global));*/
        /*script_thread.queue_finish_job(finish_job_handler, global);*/
    }
}


fn reject_promise_job(reason: &str,
                      script_thread: &ScriptThread,
                      global: &GlobalScope,
                      for_scope: &ServoUrl) {
    let err_type = Error::Type(reason.to_owned());
    let settle_type = SettleType::Reject(err_type);
    let async_job_handler = AsyncJobHandler::new(for_scope.clone(), InvokeType::Settle(settle_type));
    script_thread.queue_serviceworker_job(box async_job_handler, global);
} 

// Step 7 (perform the fetch hook)
pub fn perform_fetch_hook_and_fetch(req:Request,
                                    is_top_level: bool,
                                    use_cache: bool,
                                    force_bypass_cache: bool,
                                    reg: &ServiceWorkerRegistration, 
                                    global: &GlobalScope,
                                    reg_scope: &ServoUrl) {
    println!("Perform fetch async called");
    // Step 1
    if !use_cache || force_bypass_cache || (reg.update_time_check().is_some() && reg.update_time_check().unwrap()) {
        req.cache_mode.set(CacheMode::NoCache);
    } else {
        // Step 2
        req.skip_service_worker.set(true);
        // Step 3
        if !is_top_level {
            fetch_async(req, global, reg_scope);
            return;
        }
        // Step 4 
        req.headers_mut().set_raw("ServiceWorker", vec!["script".bytes().collect()]);
        // Step 5
        req.redirect_mode.set(RedirectMode::Error);
    }
    // Step 6
    fetch_async(req, global, reg_scope);
}


// Represents the eventual completion of a service worker script resource update
struct SWScriptFetchHandler {
    /// The response body received to date.
    data: Vec<u8>,
    /// The response metadata received to date.
    metadata: Option<Metadata>,
    /// The initial URL requested (The script url).
    url: ServoUrl,
    /// Indicates whether the request failed, and why.
    status: Result<(), NetworkError>,
    /// global object used to queue reject/resolve promise.
    global: Trusted<GlobalScope>,
    /// The scope of the registration from which this resource update was requested.
    reg_scope: ServoUrl
}

impl PreInvoke for SWScriptFetchHandler {}

impl FetchResponseListener for SWScriptFetchHandler {
    fn process_request_body(&mut self) {
        // TODO
    }

    fn process_request_eof(&mut self) {
        // TODO
    }

    #[allow(unrooted_must_root)]
    fn process_response(&mut self, fetch_metadata: Result<FetchMetadata, NetworkError>) {
        // we need script thread handle here
        println!("Async SW response runing here");
        // use global script chan to queue resolve/reject promise job
        let global = &*self.global.root();
        self.metadata = fetch_metadata.ok().map(|meta| match meta {
            FetchMetadata::Unfiltered(m) => m,
            FetchMetadata::Filtered { unsafe_, .. } => unsafe_
        });

        let mut content_type = self.metadata.as_ref().and_then(|m| {
            m.headers.as_ref().and_then(|headers| headers.get::<ContentType>())
        });
        // okay here we have extracted the mime 
        // Step 7 (Async part)
        let &ContentType(ref extracted_mime) = content_type.take().unwrap();
        println!("{:?}", extracted_mime);
        match *extracted_mime {
            Mime(TopLevel::Application , SubLevel::Javascript, _) |
            Mime(TopLevel::Text, SubLevel::Javascript, _) => {}
            _ => {
                let err_type = Error::Type("Security Error".to_owned());
                let settle_type = SettleType::Reject(err_type);
                let async_job_handler = AsyncJobHandler::new(self.url.clone(), InvokeType::Settle(settle_type));
                let dom_task = CommonScriptMsg::RunnableMsg(ScriptThreadEventCategory::ServiceWorkerEvent, box async_job_handler);
                let _ = global.script_chan().send(dom_task);
                // set our fetch context status 
                self.status = Err(NetworkError::Internal("Security Error".to_owned()));
                return;
            }
        }

        // Existence of this field allows to override the controlling scope in registration object
        let serviceworker_allowed = self.metadata.as_ref().map(|m|{
            m.headers.as_ref().and_then(|headers| headers.get_raw("Service-Worker-Allowed"))
        });

        // Step 9
        let https_state = self.metadata.as_ref().map(|m| m.https_state);

        // Step 10 TODO get referrer policy

        // Step 11 TODO check sw allowed scope failure

        // Step 12
        let ref scope_url = self.reg_scope;
        let mut max_scope_str = "";

        // modifiy the controlled scope accordingly if sw allowed header is sent with repsonse
        // TODO convert serviceworker_allowed to a &str, it is &[Vec<u8>]
        let serviceworker_allowed = serviceworker_allowed.unwrap().unwrap().to_vec()[0].clone();
        let serviceworker_allowed = String::from_utf8(serviceworker_allowed);
        let max_scope_string = ServoUrl::from_url(Url::parse("https://github.com/creativcoder/rust.json").unwrap());
        /*let max_scope_string = match serviceworker_allowed {
            None => {
                let mut url = self.url.clone();
                url.into_url().unwrap().path_segments_mut().unwrap().pop();
                url
                // Step 14
            }
            Some(scope) => {
                // Step 15
                let mut script_url = self.url.clone();
                self.url.join(scope).unwrap()
            }
        };*/

        // Step 18
        if !(max_scope_string.path() == self.reg_scope.path()) {
            let err_type = Error::Type("Security Error".to_owned());
            let settle_type = SettleType::Reject(err_type);
            let async_job_handler = AsyncJobHandler::new(self.url.clone(), InvokeType::Settle(settle_type));
            let dom_task = CommonScriptMsg::RunnableMsg(ScriptThreadEventCategory::ServiceWorkerEvent, box async_job_handler);
            let _ = global.script_chan().send(dom_task);
            self.status = Err(NetworkError::Internal("Security Error".to_owned()));
            return;
        }

        // Step 19 TODO(creativcoder) once https://github.com/whatwg/fetch/issues/376 is resolved
        
        // Step 20
        // return true;

        // this methods completion value is the script resource that will get stored in ServiceWorker object
        // TODO

    }

    fn process_response_chunk(&mut self, mut chunk: Vec<u8>) {
        // self.data will be our updated service worker script resource
        self.data.append(&mut chunk);
    }

    fn process_response_eof(&mut self, _response: Result<(), NetworkError>) {
        /*let response = self.response_object.root();
        let global = response.global();
        let cx = global.get_cx();
        let _ac = JSAutoCompartment::new(cx, global.reflector().get_jsobject().get());
        response.finish(mem::replace(&mut self.body, vec![]));
        // TODO
        // ... trailerObject is not supported in Servo yet.*/
    }
}

#[allow(unrooted_must_root)]
fn fetch_async(req: Request ,global: &GlobalScope, reg_scope: &ServoUrl) {
    println!("fetch async called core resource thread creating");
    // used to send the fetch message
    let core_resource_thread = global.core_resource_thread();
    // let response = Response::new(global);
    let (action_sender, action_receiver) = ipc::channel().unwrap();
    let sw_script_fetch = Arc::new(Mutex::new(SWScriptFetchHandler {
        data: vec![],
        url: req.url(),
        metadata: None,
        status: Ok(()),
        global: Trusted::new(global),
        reg_scope: reg_scope.clone()
    }));

    let listener = NetworkListener {
        context: sw_script_fetch,
        task_source: global.networking_task_source(),
        wrapper: Some(global.get_runnable_wrapper())
    };
    // Use the action receiver and route it to the listener we created above
    // Here message is of type FetchResponseMsg
    ROUTER.add_route(action_receiver.to_opaque(), box move |message| {
        listener.notify_fetch(message.to().unwrap());
    });
    let _ = core_resource_thread.send(WorkerScriptFetchMsg(request_init_from_request(req), action_sender));
}
