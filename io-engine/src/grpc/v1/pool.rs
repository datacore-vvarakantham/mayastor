use crate::{
    core::Share,
    grpc::{rpc_submit, GrpcClientContext, GrpcResult, RWLock, RWSerializer},
    lvs::{Error as LvsError, Lvs},
    pool_backend::{PoolArgs, PoolBackend},
};
use ::function_name::named;
use futures::FutureExt;
use io_engine_api::v1::pool::*;
use nix::errno::Errno;
use std::{convert::TryFrom, fmt::Debug, panic::AssertUnwindSafe};
use tonic::{Request, Response, Status};

#[derive(Debug)]
struct UnixStream(tokio::net::UnixStream);

/// RPC service for mayastor pool operations
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolService {
    name: String,
    client_context:
        std::sync::Arc<tokio::sync::RwLock<Option<GrpcClientContext>>>,
}

#[async_trait::async_trait]
impl<F, T> RWSerializer<F, T> for PoolService
where
    T: Send + 'static,
    F: core::future::Future<Output = Result<T, Status>> + Send + 'static,
{
    async fn locked(&self, ctx: GrpcClientContext, f: F) -> Result<T, Status> {
        let mut context_guard = self.client_context.write().await;

        // Store context as a marker of to detect abnormal termination of the
        // request. Even though AssertUnwindSafe() allows us to
        // intercept asserts in underlying method strategies, such a
        // situation can still happen when the high-level future that
        // represents gRPC call at the highest level (i.e. the one created
        // by gRPC server) gets cancelled (due to timeout or somehow else).
        // This can't be properly intercepted by 'locked' function itself in the
        // first place, so the state needs to be cleaned up properly
        // upon subsequent gRPC calls.
        if let Some(c) = context_guard.replace(ctx) {
            warn!("{}: gRPC method timed out, args: {}", c.id, c.args);
        }

        let fut = AssertUnwindSafe(f).catch_unwind();
        let r = fut.await;

        // Request completed, remove the marker.
        let ctx = context_guard.take().expect("gRPC context disappeared");

        match r {
            Ok(r) => r,
            Err(_e) => {
                warn!("{}: gRPC method panicked, args: {}", ctx.id, ctx.args);
                Err(Status::cancelled(format!(
                    "{}: gRPC method panicked",
                    ctx.id
                )))
            }
        }
    }

    async fn shared(&self, ctx: GrpcClientContext, f: F) -> Result<T, Status> {
        let context_guard = self.client_context.read().await;

        if let Some(c) = context_guard.as_ref() {
            warn!("{}: gRPC method timed out, args: {}", c.id, c.args);
        }

        let fut = AssertUnwindSafe(f).catch_unwind();
        let r = fut.await;

        match r {
            Ok(r) => r,
            Err(_e) => {
                warn!("{}: gRPC method panicked, args: {}", ctx.id, ctx.args);
                Err(Status::cancelled(format!(
                    "{}: gRPC method panicked",
                    ctx.id
                )))
            }
        }
    }
}

#[async_trait::async_trait]
impl RWLock for PoolService {
    async fn rw_lock(&self) -> &tokio::sync::RwLock<Option<GrpcClientContext>> {
        self.client_context.as_ref()
    }
}

impl TryFrom<CreatePoolRequest> for PoolArgs {
    type Error = LvsError;
    fn try_from(args: CreatePoolRequest) -> Result<Self, Self::Error> {
        if args.disks.is_empty() {
            return Err(LvsError::Invalid {
                source: Errno::EINVAL,
                msg: "invalid argument, missing devices".to_string(),
            });
        }

        if let Some(s) = args.uuid.clone() {
            let _uuid = uuid::Uuid::parse_str(s.as_str()).map_err(|e| {
                LvsError::Invalid {
                    source: Errno::EINVAL,
                    msg: format!("invalid uuid provided, {e}"),
                }
            })?;
        }

        Ok(Self {
            name: args.name,
            disks: args.disks,
            uuid: args.uuid,
            cluster_size: args.cluster_size,
        })
    }
}

impl TryFrom<ImportPoolRequest> for PoolArgs {
    type Error = LvsError;
    fn try_from(args: ImportPoolRequest) -> Result<Self, Self::Error> {
        if args.disks.is_empty() {
            return Err(LvsError::Invalid {
                source: Errno::EINVAL,
                msg: "invalid argument, missing devices".to_string(),
            });
        }

        if let Some(s) = args.uuid.clone() {
            let _uuid = uuid::Uuid::parse_str(s.as_str()).map_err(|e| {
                LvsError::Invalid {
                    source: Errno::EINVAL,
                    msg: format!("invalid uuid provided, {e}"),
                }
            })?;
        }

        Ok(Self {
            name: args.name,
            disks: args.disks,
            uuid: args.uuid,
            cluster_size: None,
        })
    }
}

impl Default for PoolService {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolService {
    pub fn new() -> Self {
        Self {
            name: String::from("PoolSvc"),
            client_context: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
        }
    }
}

impl From<Lvs> for Pool {
    fn from(l: Lvs) -> Self {
        Self {
            uuid: l.uuid(),
            name: l.name().into(),
            disks: vec![l
                .base_bdev()
                .bdev_uri_str()
                .unwrap_or_else(|| "".into())],
            state: PoolState::PoolOnline.into(),
            capacity: l.capacity(),
            used: l.used(),
            committed: l.committed(),
            pooltype: PoolType::Lvs as i32,
            cluster_size: l.blob_cluster_size() as u32,
        }
    }
}

#[tonic::async_trait]
impl PoolRpc for PoolService {
    #[named]
    async fn create_pool(
        &self,
        request: Request<CreatePoolRequest>,
    ) -> GrpcResult<Pool> {
        self.locked(
            GrpcClientContext::new(&request, function_name!()),
            async move {
                let args = request.into_inner();
                info!("{:?}", args);
                match PoolBackend::try_from(args.pooltype)? {
                    PoolBackend::Lvs => {
                        let rx = rpc_submit::<_, _, LvsError>(async move {
                            let pool = Lvs::create_or_import(
                                PoolArgs::try_from(args)?,
                            )
                            .await?;
                            Ok(Pool::from(pool))
                        })?;

                        rx.await
                            .map_err(|_| Status::cancelled("cancelled"))?
                            .map_err(Status::from)
                            .map(Response::new)
                    }
                }
            },
        )
        .await
    }

    #[named]
    async fn destroy_pool(
        &self,
        request: Request<DestroyPoolRequest>,
    ) -> GrpcResult<()> {
        self.locked(
            GrpcClientContext::new(&request, function_name!()),
            async move {
                let args = request.into_inner();
                info!("{:?}", args);
                let rx = rpc_submit::<_, _, LvsError>(async move {
                    if let Some(pool) = Lvs::lookup(&args.name) {
                        if args.uuid.is_some() && args.uuid != Some(pool.uuid())
                        {
                            return Err(LvsError::Invalid {
                                source: Errno::EINVAL,
                                msg: format!(
                                    "invalid uuid {}, found pool with uuid {}",
                                    args.uuid.unwrap(),
                                    pool.uuid(),
                                ),
                            });
                        }
                        pool.destroy().await?;
                    } else {
                        return Err(LvsError::PoolNotFound {
                            source: Errno::EINVAL,
                            msg: format!(
                                "Destroy failed as pool {} was not found",
                                args.name,
                            ),
                        });
                    }
                    Ok(())
                })?;

                rx.await
                    .map_err(|_| Status::cancelled("cancelled"))?
                    .map_err(Status::from)
                    .map(Response::new)
            },
        )
        .await
    }

    #[named]
    async fn export_pool(
        &self,
        request: Request<ExportPoolRequest>,
    ) -> GrpcResult<()> {
        self.locked(
            GrpcClientContext::new(&request, function_name!()),
            async move {
                let args = request.into_inner();
                info!("{:?}", args);
                let rx = rpc_submit::<_, _, LvsError>(async move {
                    if let Some(pool) = Lvs::lookup(&args.name) {
                        if args.uuid.is_some() && args.uuid != Some(pool.uuid())
                        {
                            return Err(LvsError::Invalid {
                                source: Errno::EINVAL,
                                msg: format!(
                                    "invalid uuid {}, found pool with uuid {}",
                                    args.uuid.unwrap(),
                                    pool.uuid(),
                                ),
                            });
                        }
                        pool.export().await?;
                    } else {
                        return Err(LvsError::Invalid {
                            source: Errno::EINVAL,
                            msg: format!("pool {} not found", args.name),
                        });
                    }
                    Ok(())
                })?;

                rx.await
                    .map_err(|_| Status::cancelled("cancelled"))?
                    .map_err(Status::from)
                    .map(Response::new)
            },
        )
        .await
    }

    #[named]
    async fn import_pool(
        &self,
        request: Request<ImportPoolRequest>,
    ) -> GrpcResult<Pool> {
        self.locked(
            GrpcClientContext::new(&request, function_name!()),
            async move {
                let args = request.into_inner();
                info!("{:?}", args);
                let rx = rpc_submit::<_, _, LvsError>(async move {
                    let pool = Lvs::import_from_args(PoolArgs::try_from(args)?)
                        .await?;
                    Ok(Pool::from(pool))
                })?;

                rx.await
                    .map_err(|_| Status::cancelled("cancelled"))?
                    .map_err(Status::from)
                    .map(Response::new)
            },
        )
        .await
    }

    #[named]
    async fn list_pools(
        &self,
        request: Request<ListPoolOptions>,
    ) -> GrpcResult<ListPoolsResponse> {
        self.locked(
            GrpcClientContext::new(&request, function_name!()),
            async move {
                let args = request.into_inner();
                let pool_type = match &args.pooltype {
                    Some(pool_type) => pool_type.value,
                    None => PoolType::Lvs as i32,
                };
                if pool_type != PoolType::Lvs as i32 {
                    return Err(tonic::Status::invalid_argument(
                        "Only pools of Lvs pool type are supported",
                    ));
                }

                let rx = rpc_submit::<_, _, LvsError>(async move {
                    let mut pools = Vec::new();
                    if let Some(name) = args.name {
                        if let Some(l) = Lvs::lookup(&name) {
                            pools.push(l.into());
                        }
                    } else if let Some(uuid) = args.uuid {
                        if let Some(l) = Lvs::lookup_by_uuid(&uuid) {
                            pools.push(l.into());
                        }
                    } else {
                        Lvs::iter().for_each(|l| pools.push(l.into()));
                    }
                    Ok(ListPoolsResponse {
                        pools,
                    })
                })?;

                rx.await
                    .map_err(|_| Status::cancelled("cancelled"))?
                    .map_err(Status::from)
                    .map(Response::new)
            },
        )
        .await
    }
}
