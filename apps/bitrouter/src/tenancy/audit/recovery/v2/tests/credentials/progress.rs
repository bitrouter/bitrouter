//! K's scoped allowlist: the existing credential counter and fixed owner phases.
use std::time::Instant;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context};

const TARGET: &str = "bitrouter::tenancy::store::directory::pages";
const OWNER_TARGET: &str = "bitrouter::offline::diagnostic";
const OWNER_PHASES: &[&str] = &[
    "streamed_owner_close_begin",
    "streamed_owner_close_returned",
    "streamed_owner_joined",
    "recovery_owner_close_begin",
    "recovery_owner_close_returned",
    "recovery_owner_joined",
    "recovery_bundle_verify_begin",
    "recovery_target_audit_begin",
    "recovery_target_cleanup_begin",
    "recovery_target_close_acked",
    "recovery_source_audit_begin",
    "recovery_source_cleanup_begin",
    "recovery_source_close_acked",
    "recovery_bundle_reverify_begin",
];

pub(super) struct CredentialProgress {
    started: Instant,
}

impl CredentialProgress {
    pub(super) fn new(started: Instant) -> Self {
        Self { started }
    }
}

#[derive(Default)]
struct Counter {
    credentials: bool,
    in_progress: bool,
    rows: Option<u64>,
}

impl Visit for Counter {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "phase" => self.credentials = value == "credentials",
            "outcome" => self.in_progress = value == "in_progress",
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "validated_rows" {
            self.rows = Some(value);
        }
    }

    // Never format the original message, an error, or any other Debug value.
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Default)]
struct OwnerPhase {
    phase: Option<&'static str>,
    confirmed: Option<bool>,
    joined: Option<bool>,
}

impl Visit for OwnerPhase {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "phase" {
            self.phase = OWNER_PHASES.iter().copied().find(|phase| *phase == value);
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        match field.name() {
            "confirmed" => self.confirmed = Some(value),
            "joined" => self.joined = Some(value),
            _ => {}
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for CredentialProgress {
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        metadata.is_event() && matches!(metadata.target(), TARGET | OWNER_TARGET)
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == OWNER_TARGET {
            let mut fields = OwnerPhase::default();
            event.record(&mut fields);
            let Some(phase) = fields.phase else { return };
            let elapsed = self.started.elapsed().as_millis();
            match phase {
                "streamed_owner_close_returned" | "recovery_owner_close_returned" => {
                    if let Some(confirmed) = fields.confirmed {
                        eprintln!(
                            "credential diagnostic phase={phase} confirmed={confirmed} elapsed_ms={elapsed}"
                        );
                    }
                }
                "streamed_owner_joined" | "recovery_owner_joined" => {
                    if let Some(joined) = fields.joined {
                        eprintln!(
                            "credential diagnostic phase={phase} joined={joined} elapsed_ms={elapsed}"
                        );
                    }
                }
                _ => eprintln!("credential diagnostic phase={phase} elapsed_ms={elapsed}"),
            }
            return;
        }
        if event.metadata().target() != TARGET {
            return;
        }
        let mut counter = Counter::default();
        event.record(&mut counter);
        if counter.credentials
            && counter.in_progress
            && let Some(rows) = counter.rows
        {
            eprintln!(
                "credential diagnostic phase=credentials outcome=in_progress validated_rows={rows} elapsed_ms={}",
                self.started.elapsed().as_millis()
            );
        }
    }
}
