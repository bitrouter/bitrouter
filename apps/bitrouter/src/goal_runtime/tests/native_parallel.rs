//! Actual native streaming, isolated writes and checked dependency consumption.
use super::*;
use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::{Request, ResponseTemplate};

fn check(id: &str, path: &str, text: &str) -> Value {
    json!({"id":id,"description":format!("Verify {path}"),"recipe":{"kind":"file_contains","path":path,"text":text},"timeout_seconds":10})
}

fn interval(runtime: &Value) -> Option<(String, u64, u64)> {
    runtime["model_work"]["entries"]
        .as_array()?
        .iter()
        .find_map(|entry| {
            let observed = &entry["observation"]["stream_activity"];
            if entry["observation"]["actual_model"] != "test-model"
                || observed["content_parts"].as_u64()? < 2
            {
                return None;
            }
            Some((
                observed["clock_id"].as_str()?.to_owned(),
                observed["first_received_tick_ns"].as_u64()?,
                observed["last_received_tick_ns"].as_u64()?,
            ))
        })
}

fn model(request: &Request) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    if body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
    {
        let text = String::from_utf8_lossy(&request.body);
        let results = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"] == "tool")
            .collect::<Vec<_>>();
        let observed = |id| results.iter().any(|message| message["tool_call_id"] == id);
        let call = |id, name, arguments: Value| json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}});
        let calls =
            if text.contains("NATIVE_PARALLEL_LEFT") || text.contains("NATIVE_PARALLEL_RIGHT") {
                if !results.is_empty() {
                    vec![]
                } else {
                    let (branch, content) = if text.contains("NATIVE_PARALLEL_LEFT") {
                        ("left", "LEFT\n")
                    } else {
                        ("right", "RIGHT\n")
                    };
                    vec![
                        call(
                            "branch-output",
                            "write",
                            json!({"path":format!("{branch}.txt"),"content":content}),
                        ),
                        call(
                            "branch-private",
                            "write",
                            json!({"path":".native-local","content":branch}),
                        ),
                    ]
                }
            } else if text.contains("NATIVE_PARALLEL_JOIN") {
                if observed("write-join") {
                    vec![]
                } else if observed("read-left") && observed("read-right") {
                    if !results.iter().any(|message| {
                        message["tool_call_id"] == "read-left"
                            && message["content"].to_string().contains("LEFT")
                    }) || !results.iter().any(|message| {
                        message["tool_call_id"] == "read-right"
                            && message["content"].to_string().contains("RIGHT")
                    }) {
                        return ResponseTemplate::new(422);
                    }
                    vec![call(
                        "write-join",
                        "write",
                        json!({"path":"joined.txt","content":"LEFT\nRIGHT\n"}),
                    )]
                } else {
                    vec![
                        call("read-left", "read", json!({"path":"left.txt"})),
                        call("read-right", "read", json!({"path":"right.txt"})),
                    ]
                }
            } else {
                return ResponseTemplate::new(400);
            };
        return if calls.is_empty() {
            native::completion(
                &body,
                json!({"role":"assistant","content":"Actual tools returned; host checks and independent review remain required."}),
                "stop",
            )
        } else {
            native::completion(
                &body,
                json!({"role":"assistant","tool_calls":calls}),
                "tool_calls",
            )
        };
    }
    let Some(state) = provider::model_state(request) else {
        return ResponseTemplate::new(400);
    };
    if state.get("available_harnesses").is_some() {
        if state["available_harnesses"] != json!([]) || state["built_in_executor_available"] != true
        {
            return ResponseTemplate::new(422);
        }
        let tasks = [("left",vec![],"left.txt","LEFT\n"),("right",vec![],"right.txt","RIGHT\n"),("join",vec!["left","right"],"joined.txt","LEFT\nRIGHT\n")]
            .map(|(id,dependencies,file,text)| json!({"id":id,"title":id,"harness":null,"dependencies":dependencies,
                "instruction":format!("NATIVE_PARALLEL_{}: {}", id.to_uppercase(), if id == "join" {
                    "Read left.txt and right.txt with actual tools before writing joined.txt as LEFT newline RIGHT newline."
                } else if id == "left" { "Write left.txt as LEFT newline and the ignored .native-local as left." }
                else { "Write right.txt as RIGHT newline and the ignored .native-local as right." }),
                "checks":[check(&format!("{id}-written"),file,text)]}));
        let plan = json!({"tasks":tasks,"completion":{"conditions":[{"id":"delivery","description":"Actual native branch overlap, isolated outputs and verified dependency join"}],
            "checks":[check("has-left","left.txt","LEFT\n"),check("has-right","right.txt","RIGHT\n"),check("has-joined","joined.txt","LEFT\nRIGHT\n")],"human_acceptance_required":false}});
        return native::completion(
            &body,
            json!({"role":"assistant","content":plan.to_string()}),
            "stop",
        );
    }
    if state.get("integrated_material").is_some() {
        let runtime = |id| {
            state["task_evidence"]
                .as_array()?
                .iter()
                .find(|task| task["task"]["id"] == id)
                .map(|task| &task["runtime_observations"])
        };
        let overlap = runtime("left")
            .and_then(interval)
            .zip(runtime("right").and_then(interval))
            .is_some_and(|((epoch, a, b), (other, c, d))| epoch == other && a < d && c < b);
        if !overlap {
            return ResponseTemplate::new(422).set_body_json(json!({"error":"actual native content intervals do not overlap","task_evidence":state["task_evidence"]}));
        }
    }
    provider::model(request)
}

struct StreamingBarrier {
    uri: String,
    server: JoinHandle<()>,
}
impl Drop for StreamingBarrier {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl StreamingBarrier {
    async fn start(backend: String) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let uri = format!("http://{}", listener.local_addr()?);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let client = reqwest::Client::new();
        let router = axum::Router::new().route("/v1/chat/completions",axum::routing::post(move |body:Bytes| {
            let backend=backend.clone(); let barrier=barrier.clone(); let client=client.clone();
            async move {
                let result = async {
                    let value:Value=serde_json::from_slice(&body)?;
                    let text=String::from_utf8_lossy(&body);
                    let independent = value["tools"].as_array().is_some_and(|tools| !tools.is_empty())
                        && (text.contains("NATIVE_PARALLEL_LEFT") || text.contains("NATIVE_PARALLEL_RIGHT"))
                        && value["messages"].as_array().is_some_and(|messages| !messages.iter().any(|message| message["role"] == "tool"));
                    let response=client.post(format!("{backend}/v1/chat/completions")).body(body).send().await?;
                    let status=response.status(); let content_type=response.headers().get(CONTENT_TYPE).cloned();
                    let bytes=response.bytes().await?;
                    let mut response=if independent && status.is_success() {
                        let first=json!({"id":"native-goal-response","object":"chat.completion.chunk","model":"test-model",
                            "choices":[{"index":0,"delta":{"content":"Received actual branch content."},"finish_reason":null}]});
                        let first=Bytes::from(format!("data: {first}\n\n"));
                        let stream=futures::stream::once(async move { Ok::<_,std::convert::Infallible>(first) }).chain(
                            futures::stream::once(async move {
                                barrier.wait().await;
                                // Both first chunks have been sent before either
                                // final tool-call chunk. Actual receipt, rather
                                // than this server barrier, is asserted below.
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                Ok::<_,std::convert::Infallible>(bytes)
                            }));
                        Response::new(Body::from_stream(stream))
                    } else { Response::new(Body::from(bytes)) };
                    *response.status_mut()=status;
                    if let Some(value)=content_type { response.headers_mut().insert(CONTENT_TYPE,value); }
                    Ok::<_,anyhow::Error>(response)
                }.await;
                match result { Ok(response)=>response,Err(error)=>(StatusCode::BAD_GATEWAY,error.to_string()).into_response() }
            }
        }));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok(Self { uri, server })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_branches_overlap_isolate_conflicting_local_writes_and_read_checked_dependencies()
-> Result<()> {
    let mut setup = fixture::Setup::new_without_external_harnesses().await?;
    tokio::fs::write(setup.workspace.join(".gitignore"), ".native-local\n").await?;
    git(&setup.workspace, &["add".as_ref(), ".gitignore".as_ref()]).await?;
    git(
        &setup.workspace,
        &[
            "-c".as_ref(),
            "user.name=Goal Fixture".as_ref(),
            "-c".as_ref(),
            "user.email=goal-fixture@example.test".as_ref(),
            "-c".as_ref(),
            "core.hooksPath=/dev/null".as_ref(),
            "commit".as_ref(),
            "--no-gpg-sign".as_ref(),
            "-qm".as_ref(),
            "native isolation fixture".as_ref(),
        ],
    )
    .await?;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(model)
        .with_priority(1)
        .mount(&setup.backend)
        .await;
    let proxy = StreamingBarrier::start(setup.backend.uri()).await?;
    setup.review_api(&proxy.uri);
    let fixture = setup.start().await?;
    let result=async {
        let id=fixture.create_with("Deliver LEFT and RIGHT files with actual overlapping native branches in isolated workspaces, then read their checked contents and join them. Keep ignored branch-local scratch files isolated. Verify all delivery files; no external harness or human acceptance is needed.").await?;
        // Each dependency stage has its own 45s wait: concurrent branches,
        // their dependent join, then root acceptance. Combining both worker
        // stages exceeded 45s on Linux after the join Turn had completed.
        wait_for(&fixture.socket,&id,|goal| goal.workflow.tasks.len() == 3
            && goal.workflow.tasks.iter().filter(|task| task.spec.id != "join").all(|task|
                task.executions.last().is_some_and(|execution| execution.handoff.is_some()))).await?;
        wait_for(&fixture.socket,&id,|goal| goal.workflow.tasks.len() == 3
            && goal.workflow.tasks.iter().all(|task| task.executions.last()
                .is_some_and(|execution| execution.handoff.is_some()))).await?;
        let goal=wait_for(&fixture.socket,&id,|goal| goal.workflow.completion.completed_for(1)).await?;
        let task=|id| goal.workflow.tasks.iter().find(|task| task.spec.id == id).context("native task missing");
        let left=task("left")?; let right=task("right")?; let join=task("join")?;
        let execution=|task:&bitrouter_orchestrator::goal::workflow::Task| task.executions.last().cloned().context("native attempt missing");
        let a=execution(left)?;let b=execution(right)?;let joined=execution(join)?;
        assert_ne!(a.workspace.canonicalize()?,b.workspace.canonicalize()?);
        assert_eq!(tokio::fs::read_to_string(a.workspace.join(".native-local")).await?,"left");
        assert_eq!(tokio::fs::read_to_string(b.workspace.join(".native-local")).await?,"right");
        assert!(!fixture.workspace.join(".native-local").exists());
        assert!(!a.workspace.join("right.txt").exists());assert!(!b.workspace.join("left.txt").exists());
        assert_eq!(tokio::fs::read_to_string(joined.workspace.join("joined.txt")).await?,"LEFT\nRIGHT\n");
        let mut intervals=Vec::new();
        for (task,execution) in [(left,&a),(right,&b)] {
            assert_eq!(task.executions.len(),1);
            let source=super::super::native::source(&goal,task,execution)?;
            let evidence=super::super::native_evidence::model_work(&fixture.goals,&fixture.root(&id),&goal,&source).await?;
            intervals.push(interval(&json!({"model_work":evidence})).context("actual content interval missing")?);
        }
        assert_eq!(intervals[0].0, intervals[1].0);
        assert!(intervals[0].1 < intervals[1].2 && intervals[1].1 < intervals[0].2);
        let prepared=joined.resources.as_ref().context("joined resources missing")?;
        let reference=prepared.checked_dependencies.as_ref().context("checked dependencies missing")?;
        let bytes=crate::goal_artifacts::read(&fixture.root(&id),reference).await?;
        let dependencies:Value=serde_json::from_slice(&bytes)?;
        assert_eq!(dependencies.as_array().context("dependency list missing")?.len(),2);
        for upstream in [left,right] {
            let current=upstream.executions.last().and_then(|execution| execution.handoff.as_ref()).context("upstream handoff missing")?;
            let expected=serde_json::to_value(current)?;
            assert!(dependencies.as_array().into_iter().flatten().any(|dependency| dependency["task_id"] == upstream.spec.id
                && dependency["checked_handoff"] == expected));
        }
        let requests=fixture.backend.received_requests().await.context("captured requests missing")?;
        assert!(requests.iter().any(|request| serde_json::from_slice::<Value>(&request.body).ok().is_some_and(|body|
            body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message| message["tool_call_id"] == "read-left")
                && messages.iter().any(|message| message["tool_call_id"] == "read-right")))));
        Ok(())
    }.await;
    let result = fixture.finish(result).await;
    drop(proxy);
    result
}
