use crate::worker_ctx::{create_user_worker_pool, create_worker, WorkerRequestMsg};
use anyhow::{anyhow, bail, Error};
use hyper::{server::conn::Http, service::Service, Body, Request, Response};
use log::{debug, error, info};
use sb_worker_context::essentials::{EdgeContextInitOpts, EdgeContextOpts, EdgeMainRuntimeOpts};
use std::future::Future;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::str;
use std::str::FromStr;
use std::task::Poll;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

struct WorkerService {
    worker_req_tx: mpsc::UnboundedSender<WorkerRequestMsg>,
}

impl WorkerService {
    fn new(worker_req_tx: mpsc::UnboundedSender<WorkerRequestMsg>) -> Self {
        Self { worker_req_tx }
    }
}

impl Service<Request<Body>> for WorkerService {
    type Response = Response<Body>;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // create a response in a future.
        let worker_req_tx = self.worker_req_tx.clone();
        let fut = async move {
            let req_path = req.uri().path();

            // if the request is for the health endpoint return a 200 OK response
            if req_path == "/_internal/health" {
                return Ok(Response::new(Body::empty()));
            }

            let (res_tx, res_rx) = oneshot::channel::<Result<Response<Body>, hyper::Error>>();
            let msg = WorkerRequestMsg { req, res_tx };

            if worker_req_tx.send(msg).is_err() {
                bail!("main worker request channel is closed")
            }

            let result = res_rx.await;
            if result.is_err() {
                bail!("failed to get a response from main worker")
            }

            match result.unwrap() {
                Ok(res) => Ok(res),
                Err(e) => {
                    error!("received an error for request {:?}", e);
                    //Err(anyhow!(e))
                    // FIXME return an error status
                    Ok(Response::new(Body::empty()))
                }
            }
        };

        // Return the response as an immediate future
        Box::pin(fut)
    }
}

pub struct Server {
    ip: Ipv4Addr,
    port: u16,
    main_worker_req_tx: mpsc::UnboundedSender<WorkerRequestMsg>,
}

impl Server {
    pub async fn new(
        ip: &str,
        port: u16,
        main_service_path: String,
        import_map_path: Option<String>,
        no_module_cache: bool,
    ) -> Result<Self, Error> {
        // create a user worker pool
        let user_worker_msgs_tx = create_user_worker_pool().await?;

        // create main worker
        let main_path = Path::new(&main_service_path);
        let main_worker_req_tx = create_worker(EdgeContextInitOpts {
            service_path: main_path.to_path_buf(),
            import_map_path,
            no_module_cache,
            conf: EdgeContextOpts::MainWorker(EdgeMainRuntimeOpts {
                worker_pool_tx: user_worker_msgs_tx,
            }),
            env_vars: std::env::vars().collect(),
        })
        .await?;

        let ip = Ipv4Addr::from_str(ip)?;
        Ok(Self {
            ip,
            port,
            main_worker_req_tx,
        })
    }

    pub async fn listen(&mut self) -> Result<(), Error> {
        let addr = SocketAddr::new(IpAddr::V4(self.ip), self.port);
        let listener = TcpListener::bind(&addr).await?;
        debug!("edge-runtime is listening on {:?}", listener.local_addr()?);

        loop {
            let main_worker_req_tx = self.main_worker_req_tx.clone();

            tokio::select! {
                msg = listener.accept() => {
                    match msg {
                       Ok((conn, _)) => {
                           tokio::task::spawn(async move {
                             let service = WorkerService::new(main_worker_req_tx);

                             let conn_fut = Http::new()
                                .serve_connection(conn, service);

                             if let Err(e) = conn_fut.await {
                                 error!("{:?}", e);
                             }
                           });
                       }
                       Err(e) => error!("socket error: {}", e)
                    }
                }
                // wait for shutdown signal...
                _ = tokio::signal::ctrl_c() => {
                    info!("shutdown signal received");
                    break;
                }
            }
        }
        Ok(())
    }
}
