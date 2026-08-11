#![forbid(unsafe_code)]

pub mod local_sandbox;
mod project_template;
pub mod studio_remote;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use studio_native_auth::NativeAuthConfig;

pub use project_template::{init_project_template, init_project_with_id, preflight_init_directory};

#[derive(Debug, Clone, ClapArgs)]
pub struct NativeAuthOptions {
    #[arg(
        long,
        env = "REPORCH_STUDIO_OIDC_ISSUER",
        default_value = "https://reporch.com/oauth"
    )]
    pub issuer: String,
    #[arg(
        long,
        env = "REPORCH_STUDIO_CLI_CLIENT_ID",
        default_value = "reporch-studio-cli"
    )]
    pub client_id: String,
    /// Permit plain HTTP only for a localhost development issuer.
    #[arg(
        long,
        env = "REPORCH_STUDIO_ALLOW_INSECURE_HTTP",
        default_value_t = false
    )]
    pub allow_insecure_http: bool,
}

pub fn device_auth_config(options: &NativeAuthOptions) -> Result<NativeAuthConfig> {
    NativeAuthConfig::device(
        &options.issuer,
        &options.client_id,
        vec![
            "openid".into(),
            "profile".into(),
            "offline_access".into(),
            "studio:entitlements".into(),
        ],
        options.allow_insecure_http,
    )
    .context("validate native OAuth configuration")
}
