//! Checked experiment admission of an actual owned Core model step.
use super::TenantExecution;
use crate::{harness::resources::managed::store::model::Origin, tenancy::experiment::Profile};
use bitrouter_ai::{
    core::protocol::ModelMode,
    tenancy::{configuration::LearningMode, operation::ErrorCode},
};

impl TenantExecution {
    pub(super) fn experiment_profile(&self, origin: &Origin) -> Result<Option<Profile>, ErrorCode> {
        let facts = origin.facts();
        let root_experiment = origin
            .binding()
            .authority()
            .experimental_model(&facts.run_model);
        let step_experiment = origin
            .binding()
            .authority()
            .experimental_model(&facts.model);
        if (root_experiment || step_experiment)
            && (facts.model != facts.run_model
                || facts.run_selection != ModelMode::Policy
                || facts.selection != ModelMode::Policy)
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        let Some(policy) = self.route.policy() else {
            return Ok(None);
        };
        let definition = policy
            .document()
            .policies
            .get(policy.name())
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        let Some(treatment) = definition
            .optimization
            .as_ref()
            .and_then(|o| o.active.as_ref())
        else {
            return Ok(None);
        };
        let descriptor = origin.binding().descriptor();
        let facts = origin.facts();
        if !origin.binding().belongs_to(&self.service)
            || descriptor.tenant != self.submission.tenant
            || descriptor.principal != self.submission.principal
            || descriptor.revision != self.submission.binding.execution_revision
            || facts.selection != ModelMode::Policy
            || facts.model != self.route.reference().resource_id.as_str()
            || self.learning.mode != LearningMode::Explore
            || self.choice.is_some()
            || self.route.choice_profile().is_some()
            || self.route.count_candidate()?.is_some()
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        self.route.require_static_experiment(self)?;
        let config = policy.configuration()?;
        crate::tenancy::trajectory_profile::guarded(&config, policy.document(), policy.name())
            .map_err(|_| ErrorCode::CapabilityMismatch)?;
        let policy_digest = crate::policy_lock::semantic_digest(policy.document())
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let definition_digest =
            crate::storage::evaluation::canonical_digest(&(policy.name(), definition))
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        Ok(Some(Profile {
            revision: descriptor.revision.clone(),
            route: self.route.reference().clone(),
            policy: policy.reference().clone(),
            policy_name: policy.name().to_owned(),
            policy_digest,
            definition_digest,
            treatment: treatment.clone(),
        }))
    }
}

impl TenantExecution {
    pub(in crate::tenancy) fn matches_experiment(
        &self,
        profile: &Profile,
        origin: &Origin,
    ) -> bool {
        let descriptor = origin.binding().descriptor();
        origin.binding().belongs_to(&self.service)
            && descriptor.tenant == self.submission.tenant
            && descriptor.principal == self.submission.principal
            && profile.revision == self.submission.binding.execution_revision
            && profile.route == *self.route.reference()
    }
}

impl TenantExecution {
    pub(in crate::tenancy) fn install_experiment(
        &self,
        witness: crate::tenancy::experiment::Witness,
    ) -> Result<(), ErrorCode> {
        if !witness.matches(self) {
            return Err(ErrorCode::Forbidden);
        }
        self.experiment
            .set(std::sync::Arc::new(witness))
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub(in crate::tenancy) fn experiment(
        &self,
    ) -> Option<&std::sync::Arc<crate::tenancy::experiment::Witness>> {
        self.experiment.get()
    }
    pub(super) async fn enroll_experiment(
        self: &std::sync::Arc<Self>,
        step: &super::session::StepBinding,
    ) -> crate::gateway::error::Result<()> {
        let Some(profile) = self
            .experiment_profile(step.origin())
            .map_err(|code| self.reject(code))?
        else {
            return Ok(());
        };
        crate::tenancy::experiment::queue::enroll(self, step.origin().clone(), profile)
            .await
            .map_err(|failure| self.reject(failure.0.code))
    }
}

impl TenantExecution {
    pub(crate) fn experiment_for_routing(
        &self,
    ) -> Result<Option<std::sync::Arc<crate::tenancy::experiment::Witness>>, ErrorCode> {
        self.revalidate()?;
        match self.experiment.get() {
            Some(witness) if witness.matches(self) => Ok(Some(witness.clone())),
            Some(_) => Err(ErrorCode::Forbidden),
            None => Ok(None),
        }
    }
}
