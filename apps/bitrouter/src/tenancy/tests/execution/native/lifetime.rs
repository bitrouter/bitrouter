use super::*;

#[tokio::test]
async fn admitted_native_callbacks_outlive_submission_without_renewing_a_total() -> Result<()> {
    for total in [None, Some(180)] {
        let provider = Provider::start(false).await?;
        let (mut fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        fixture.service.execution_settings.execution_timeout_secs = total;
        let gate = Arc::new(Gate::default());
        let release_on_drop = gate.release.clone().drop_guard();
        let checked = async {
            let (headers, _) = ready(&fixture, &route).await?;
            let control = Arc::new(Control {
                hold: Some((Hold::Plan, gate.clone())),
                ..Default::default()
            });
            let service = fixture.service.clone();
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                service
                    .execute_native_controlled(prompt(), headers, control)
                    .await
            }));
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(75)).await;
            tokio::time::resume();
            ensure!(!call.is_finished() && provider.calls() == 0);
            gate.release.cancel();
            tokio::time::timeout(TIMEOUT, call).await???;
            ensure!(provider.calls() == 1);
            Ok(())
        }
        .await;
        drop(release_on_drop);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}
