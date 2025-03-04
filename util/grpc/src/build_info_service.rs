// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Implementation of the BuildInfoApi service.

use crate::{
    build_info::BuildInfo,
    build_info_grpc::{create_build_info_api, BuildInfoApi},
    empty::Empty,
    rpc_logger, send_result, SVC_COUNTERS,
};
use grpcio::{RpcContext, Service, UnarySink};
use mc_common::logger::Logger;

/// A service that exposes the BuildInfo of a service recorded using
/// mc_util_build_info
#[derive(Clone)]
pub struct BuildInfoService {
    logger: Logger,
}

impl BuildInfoService {
    /// Create a new instance of the BuildInfo service
    pub fn new(logger: Logger) -> Self {
        Self { logger }
    }

    /// Convert into a grpcio::Service
    pub fn into_service(self) -> Service {
        create_build_info_api(self)
    }
}

/// Get the BuildInfo object, by reading from the BuildInfo crate
pub fn get_build_info() -> BuildInfo {
    let mut build_info = BuildInfo::new();
    build_info.set_git_commit(::mc_util_build_info::git_commit().to_owned());
    build_info.set_profile(::mc_util_build_info::profile().to_owned());
    build_info.set_debug(::mc_util_build_info::debug().to_owned());
    build_info.set_opt_level(::mc_util_build_info::opt_level().to_owned());
    build_info.set_debug_assertions(::mc_util_build_info::debug_assertions().to_owned());
    build_info.set_target_arch(::mc_util_build_info::target_arch().to_owned());
    build_info.set_target_feature(::mc_util_build_info::target_feature().to_owned());
    build_info.set_rustflags(::mc_util_build_info::rustflags().to_owned());
    build_info.set_sgx_mode(::mc_util_build_info::sgx_mode().to_owned());
    build_info.set_ias_mode(::mc_util_build_info::ias_mode().to_owned());
    build_info
}

impl BuildInfoApi for BuildInfoService {
    fn get_build_info(&mut self, ctx: RpcContext, _req: Empty, sink: UnarySink<BuildInfo>) {
        let _timer = SVC_COUNTERS.req(&ctx);
        let logger = rpc_logger(&ctx, &self.logger);
        send_result(ctx, sink, Ok(get_build_info()), &logger);
    }
}
