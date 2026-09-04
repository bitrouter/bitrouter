//! The wire half of the session's effects — the one interpreter every loop
//! shares.
//!
//! The reducer in `bitrouter_tui::machine` names what a session does as an
//! [`Effect`]; some of those are the view's and some are the wire's. The
//! interactive loop, the piped loop, and the headless `acp prompt` loop used
//! to each hand-roll the wire half — three copies of "remove the request,
//! resolve it", three of "cancel, then deny what is outstanding" — and the
//! headless one had drifted: it could only ever deny. [`Wire`] is that half,
//! written once, so a permission answered by a keystroke and one answered by a
//! headless policy reach the agent through the same code.
//!
//! What it reaches is the ACP client and nothing else. The daemon bridges the
//! launch half builds, the config, the metering store — none of it is named
//! here, and the guard in [`crate::chat`] scans this file to keep it so.

use std::collections::{HashMap, VecDeque};

use bitrouter_sdk::acp::client::{AcpClient, PendingPermission, RouteError};
use bitrouter_tui::machine::{Action, Effect, Routes};
use bitrouter_tui::permission::{Decision, Policy, Prompt};

/// The session's wire, and the requests it is holding open.
pub(crate) struct Wire<'a> {
    client: &'a AcpClient,
    session_id: &'a str,
    /// Requests admitted and not yet answered, keyed the way the machine
    /// routes answers. Entries are **removed** when answered: a retained entry
    /// holds a strong handle on the resolver, and the client's ledger holds a
    /// weak one on purpose so that a dropped request still denies itself.
    outstanding: HashMap<String, PendingPermission>,
    /// Answers produced by an effect that awaited inline, for the driver to
    /// dispatch before it selects again.
    replies: VecDeque<Action>,
}

impl<'a> Wire<'a> {
    /// A wire over one client and one harness-native session.
    pub(crate) fn new(client: &'a AcpClient, session_id: &'a str) -> Self {
        Self {
            client,
            session_id,
            outstanding: HashMap::new(),
            replies: VecDeque::new(),
        }
    }

    /// Hold a request open and hand back the prompt it is drawn and answered
    /// as. The tool kind rides along: it is the only classification the wire
    /// carries, and a headless policy reads it.
    pub(crate) fn admit(&mut self, request: PendingPermission) -> Prompt {
        let prompt = Prompt::new(
            request.request_id.clone(),
            request.tool_call.fields.title.clone(),
            request.tool_call.tool_call_id.0.to_string(),
            request.tool_call.fields.kind,
            request.options.clone(),
        );
        self.outstanding.insert(request.request_id.clone(), request);
        prompt
    }

    /// Answer a request the way a headless policy says to, on the spot.
    ///
    /// Returns the decision the agent heard and the prompt, so the caller can
    /// say what was decided and count it. The answer goes through
    /// [`Wire::apply`] like a keystroke's would.
    pub(crate) async fn answer(
        &mut self,
        request: PendingPermission,
        policy: &Policy,
    ) -> (Decision, Prompt) {
        let prompt = self.admit(request);
        let (decision, effect) = bitrouter_tui::machine::decide(policy, &prompt);
        // `decide` only ever produces a `Resolve`, which `apply` consumes.
        let _ = self.apply(effect).await;
        (decision, prompt)
    }

    /// The next answer an inline effect produced, if any.
    pub(crate) fn reply(&mut self) -> Option<Action> {
        self.replies.pop_front()
    }

    /// Run the effect if it is the wire's. Returns it untouched if it is not,
    /// for whoever owns the view.
    pub(crate) async fn apply(&mut self, effect: Effect) -> Option<Effect> {
        match effect {
            Effect::Resolve { id, outcome } => {
                if let Some(request) = self.outstanding.remove(&id) {
                    request.resolve(outcome);
                }
            }
            Effect::Cancel => {
                // Tell the agent, not merely ourselves: dropping the turn
                // future stops this side waiting and leaves the agent working.
                let told = self.client.cancel(self.session_id).await;
                // And leave no question hanging. The machine has already
                // answered whatever it was holding; this answers anything the
                // client emitted that never reached it — while the connection
                // is still live, rather than at teardown.
                self.outstanding.clear();
                self.client.deny_session_permissions(self.session_id);
                if let Err(error) = told {
                    tracing::warn!(%error, "cancelling the turn");
                }
            }
            // Awaited inline, exactly as the picker's own loop awaited it: both
            // are reachable only from an idle prompt, where no turn is
            // streaming and nothing else needs the loop.
            Effect::ListRoutes => {
                self.replies.push_back(Action::Routes(
                    match self.client.route_list(self.session_id).await {
                        Ok(listed) => Ok(Routes {
                            available: listed.available,
                            current: listed.current,
                        }),
                        Err(error) => Err(format!("{error}")),
                    },
                ));
            }
            // Typed by the client, so a refused route and a vanished binding
            // read differently without parsing text.
            Effect::SetRoute(route) => self.replies.push_back(Action::Routed(
                match self.client.route_set(self.session_id, &route).await {
                    Ok(in_force) => Ok(in_force),
                    Err(RouteError::InvalidRoute(message)) => {
                        Err(format!("route unchanged: {message}"))
                    }
                    Err(RouteError::Unavailable(message)) => Err(format!(
                        "route unchanged: route control is unavailable ({message})"
                    )),
                    Err(RouteError::Other(error)) => Err(format!("route unchanged: {error:#}")),
                },
            )),
            other => return Some(other),
        }
        None
    }
}
