-- Pre-split SQLite storage for offline capture, checkpoint and receipt projections.
-- Table and index definitions from the stopped 1ca3302f paid-recovery fixture.
CREATE TABLE "acp_assessment_revisions" ( "revision_id" varchar(64) NOT NULL, "session_key" varchar(64) NOT NULL, "checkpoint_id" varchar(64) NOT NULL, "created_at" text NOT NULL, "revision_json" text NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("revision_id") );

CREATE TABLE "acp_canonical_sessions" ( "session_key" varchar(64) NOT NULL, "owner" text NOT NULL, "source" text NOT NULL, "native_session_id" text NOT NULL, "head" bigint NOT NULL, "history_origin" text NOT NULL, "parent_key" varchar(64), "parent_watermark" bigint, "deleted" boolean NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("session_key") );

CREATE TABLE "acp_capture_connections" ( "connection_id" varchar(64) NOT NULL, "owner" text NOT NULL, "source" text NOT NULL, "controller_instance_id" text, "route_scope_id" text, "state" text NOT NULL, "head" bigint NOT NULL, "started_at" text NOT NULL, "metadata_json" text NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("connection_id") );

CREATE TABLE "acp_capture_events" ( "connection_id" varchar(64) NOT NULL, "sequence" bigint NOT NULL, "session_key" varchar(64), "session_sequence" bigint, "replay" boolean NOT NULL, "event_json" text NOT NULL, "captured_at" text NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("connection_id", "sequence") );

CREATE TABLE "acp_checkpoint_members" ( "checkpoint_id" varchar(64) NOT NULL, "session_key" varchar(64) NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("checkpoint_id", "session_key") );

CREATE TABLE "acp_checkpoints" ( "checkpoint_id" varchar(64) NOT NULL, "session_key" varchar(64) NOT NULL, "manifest_json" text NOT NULL, "watermark" bigint NOT NULL, "deleted" boolean NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("checkpoint_id") );

CREATE TABLE "acp_effective_assessments" ( "session_key" varchar(64) NOT NULL, "revision_id" varchar(64), "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("session_key") );

CREATE TABLE "acp_resource_observations" ( "observation_id" varchar(64) NOT NULL, "checkpoint_id" varchar(64) NOT NULL, "observed_at" text NOT NULL, "observation_json" text NOT NULL, "revision" bigint NOT NULL, "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, PRIMARY KEY ("observation_id") );

CREATE TABLE "checkpoint_evolution" ( "record_id" varchar(64) PRIMARY KEY, "scope_id" varchar(64) NOT NULL, "kind" varchar(32) NOT NULL, "session_key" varchar(64), "revision" bigint NOT NULL, "body" text NOT NULL , "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY);

CREATE TABLE "requests" ( "request_id" varchar NOT NULL PRIMARY KEY, "user_id" varchar NOT NULL, "api_key_id" varchar NOT NULL, "model_id" varchar NOT NULL, "provider_id" varchar NOT NULL, "prompt_tokens" bigint NOT NULL, "completion_tokens" bigint NOT NULL, "reasoning_tokens" bigint NOT NULL, "cache_read_tokens" bigint NOT NULL DEFAULT 0, "cache_write_tokens" bigint NOT NULL DEFAULT 0, "estimated_charge_micro_usd" bigint NOT NULL DEFAULT 0, "streamed" integer NOT NULL DEFAULT 0, "latency_ms" bigint NOT NULL DEFAULT 0, "generation_time_ms" bigint NOT NULL DEFAULT 0, "error" varchar, "created_at" varchar NOT NULL , "uncached_input_tokens" bigint NOT NULL DEFAULT 0, "output_tokens" bigint NOT NULL DEFAULT 0, "usage_origin" varchar NOT NULL DEFAULT 'unknown', "raw_usage_json" text, "charge_status" varchar NOT NULL DEFAULT 'legacy_unknown', "charge_evidence_json" text, "reconciliation_status" varchar NOT NULL DEFAULT 'not_applicable', "reconciliation_attempts" integer NOT NULL DEFAULT 0, "reconciliation_last_error" text, "reconciliation_last_attempt_at" varchar, "authoritative_settled_at" varchar, "authoritative_receipt_json" text, "launch_id" varchar, "agent_harness" varchar, "controller_instance_id" varchar, "acp_session_id" varchar, "native_root_session_id" varchar, "native_agent_thread_id" varchar, "native_parent_agent_thread_id" varchar, "native_turn_id" varchar, "route_lease_id" varchar, "session_identity_json" text, "route_scope_id" varchar, "router_id" varchar, "binding_digest" varchar, "original_selector" varchar);

CREATE INDEX "idx_acp_checkpoints_session" ON "acp_checkpoints" ("session_key");

CREATE INDEX "idx_acp_members_session" ON "acp_checkpoint_members" ("session_key");

CREATE INDEX "idx_acp_resources_checkpoint" ON "acp_resource_observations" ("checkpoint_id");

CREATE INDEX "idx_acp_revisions_checkpoint" ON "acp_assessment_revisions" ("checkpoint_id");

CREATE INDEX "idx_acp_revisions_session" ON "acp_assessment_revisions" ("session_key");

CREATE UNIQUE INDEX "idx_acp_session_sequence" ON "acp_capture_events" ("session_key", "session_sequence");

CREATE INDEX "idx_checkpoint_evolution_scope_kind" ON "checkpoint_evolution" ("scope_id", "kind");

CREATE INDEX "idx_checkpoint_evolution_session" ON "checkpoint_evolution" ("session_key");

CREATE INDEX "idx_legacy33_0_scope" ON "acp_capture_connections" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_1_scope" ON "acp_canonical_sessions" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_2_scope" ON "acp_capture_events" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_3_scope" ON "checkpoint_evolution" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_4_scope" ON "acp_checkpoints" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_5_scope" ON "acp_checkpoint_members" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_6_scope" ON "acp_resource_observations" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_7_scope" ON "acp_assessment_revisions" ("installation_id", "tenant_id");

CREATE INDEX "idx_legacy33_8_scope" ON "acp_effective_assessments" ("installation_id", "tenant_id");

CREATE INDEX "idx_requests_api_key_created" ON "requests" ("api_key_id", "created_at");

CREATE INDEX "idx_requests_launch_id" ON "requests" ("launch_id");

CREATE INDEX "idx_requests_user_created" ON "requests" ("user_id", "created_at");

CREATE TABLE "trajectory_requests" ( "request_id" varchar NOT NULL PRIMARY KEY, "owner_user_id" varchar NOT NULL, "episode_id" varchar NOT NULL, "start_event_id" varchar NOT NULL, "settlement_event_id" varchar, "settlement_outbox_id" varchar, "full_input_digest" varchar NOT NULL, "native_parent_id" varchar, "protocol" varchar NOT NULL, "status" varchar NOT NULL , "installation_id" varchar(128) COLLATE BINARY, "tenant_id" varchar(128) COLLATE BINARY, "creator_principal_id" varchar(128) COLLATE BINARY, "logical_identity_json" varchar(8192) COLLATE BINARY);

CREATE INDEX "idx_traj32_requests_digest" ON "trajectory_requests" ("installation_id", "tenant_id", "full_input_digest");

CREATE INDEX "idx_traj32_requests_episode" ON "trajectory_requests" ("installation_id", "tenant_id", "episode_id");

CREATE INDEX "idx_trajectory_requests_owner_episode" ON "trajectory_requests" ("owner_user_id", "episode_id");

CREATE INDEX "idx_trajectory_requests_owner_full_input_digest" ON "trajectory_requests" ("owner_user_id", "full_input_digest");
