//! Shared production process/controller/client assembly for local ACP callers.
use super::super::{
    capture::CapturePort,
    client::{AcpClient, ClientOptions},
    controller::{Controller, ControllerConfig, RouteControl, SessionCost},
    up::AgentProcess,
};
use crate::{
    acp_cli::LaunchOptions,
    config::agent::{AcpAgentConfig, AcpTransport},
};
use anyhow::Result;
use futures::FutureExt;
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct Attachments {
    pub(crate) capture: Option<Arc<dyn CapturePort>>,
    pub(crate) route: Option<Arc<dyn RouteControl>>,
    pub(crate) cost: Option<Arc<dyn SessionCost>>,
    #[cfg(feature = "core")]
    pub(crate) owned: Option<(
        Arc<super::super::up::owned::Control>,
        Arc<crate::tenancy::capture::port::Port>,
    )>,
}
pub(crate) struct Connection {
    pub(crate) connect: futures::future::BoxFuture<'static, Result<AcpClient>>,
    pub(crate) reaped: futures::future::Shared<futures::channel::oneshot::Receiver<()>>,
    #[cfg(feature = "core")]
    pub(crate) client_driver: Option<tokio::task::JoinHandle<()>>,
    pub(crate) controller: tokio::task::JoinHandle<Result<()>>,
}

pub(crate) fn start(
    agent: &AcpAgentConfig,
    config: ControllerConfig,
    options: &LaunchOptions,
    attachments: Attachments,
) -> Result<Connection> {
    agent.validate()?;
    #[cfg(feature = "core")]
    let owned = attachments.owned.is_some();
    let AcpTransport::Stdio { command, args, env } = &agent.transport;
    let process = AgentProcess::new(command.clone(), args.clone(), env.clone())
        .strip_inherited_env(options.strip_inherited_env.clone());
    #[cfg(feature = "core")]
    let process = if let Some((control, _)) = &attachments.owned {
        process.owned(control.clone())
    } else {
        process
    };
    let mut process = process;
    let reaped = process.reaped().shared();
    let mut controller = Controller::new(process, config);
    if let Some(route) = attachments.route {
        controller = controller.route_control(route);
    }
    if let Some(cost) = attachments.cost {
        controller = controller.session_cost(cost);
    }
    if let Some(capture) = attachments.capture {
        controller = controller.capture(capture);
    }
    #[cfg(feature = "core")]
    if let Some((control, port)) = attachments.owned {
        controller = controller.owned_capture(control.binding.clone(), port);
    }
    let (manager_side, controller_side) = agent_client_protocol::Channel::duplex();
    let controller = tokio::spawn(async move {
        controller
            .run(controller_side)
            .await
            .map_err(|error| anyhow::anyhow!("acp controller: {error}"))
    });
    let client_options = ClientOptions {
        turn_timeout: options.turn_timeout,
        terminal_auth: options.terminal_auth,
    };
    #[cfg(feature = "core")]
    let (connect, client_driver) = if owned {
        let (connect, driver) = AcpClient::begin_connect(manager_side, client_options);
        (connect, Some(driver))
    } else {
        let connect: futures::future::BoxFuture<'static, Result<AcpClient>> =
            Box::pin(AcpClient::connect(manager_side, client_options));
        (connect, None)
    };
    #[cfg(not(feature = "core"))]
    let connect = Box::pin(AcpClient::connect(manager_side, client_options));
    Ok(Connection {
        connect,
        #[cfg(feature = "core")]
        client_driver,
        reaped,
        controller,
    })
}
