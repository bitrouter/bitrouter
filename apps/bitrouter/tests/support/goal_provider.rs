//! Fixed provider judgments for coordinator transport tests; not calibration.
use serde_json::{Value, json};
use wiremock::{Request, ResponseTemplate};

fn check(id: &str, file: &str, text: &str) -> Value {
    json!({"id":id,"description":format!("Actual {file} contents"),
        "recipe":{"kind":"file_contains","path":file,"text":text},"timeout_seconds":10})
}
fn plan() -> Value {
    let tasks = [
        ("left", Vec::<&str>::new(), "left.txt", "LEFT\n"),
        ("right", Vec::new(), "right.txt", "RIGHT\n"),
        ("join", vec!["left","right"], "joined.txt", "LEFT\nRIGHT\n"),
    ].map(|(id, dependencies, file, text)| json!({
        "id":id,"title":format!("Fixture {id}"),"instruction":format!("Write and verify {file}"),
        "harness":"goal-peer","dependencies":dependencies,
        "checks":[check(&format!("{id}-written"),file,text)]
    }));
    json!({"tasks":tasks,"completion":{"conditions":[{"id":"delivery",
        "description":"Both independent files and the actual dependency join are delivered"}],
        "checks":[check("has-left","left.txt","LEFT\n"),check("has-right","right.txt","RIGHT\n"),
            check("has-joined","joined.txt","LEFT\nRIGHT\n")],"human_acceptance_required":false}})
}

pub fn model(request: &Request) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let state = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .and_then(|message| message["content"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    let Some(state) = state else {
        return ResponseTemplate::new(400);
    };
    let result = if state.get("available_harnesses").is_some() {
        let mut plan = plan();
        if state["goal"]
            .as_str()
            .is_some_and(|goal| goal.contains("RESOURCE_SELECTION_FIXTURE"))
        {
            let Some(skill) =
                state["available_resources"]["skills"]
                    .as_array()
                    .and_then(|skills| {
                        skills
                            .iter()
                            .find(|skill| skill["name"] == "goal-resource-fixture")
                    })
            else {
                return ResponseTemplate::new(400);
            };
            plan["tasks"][0]["resources"] = json!({"mcp_servers":["selected-resource-server"],
                "skills":[{"material_id":skill["material"]["material_id"],"sha256":skill["material"]["sha256"]}],"context_files":["seed.txt"]});
        }
        if state["goal"]
            .as_str()
            .is_some_and(|goal| goal.contains("MODEL_ROUTE_SELECTION_FIXTURE"))
        {
            let Some(route) = state["available_resources"]["model_routes"]
                .as_array()
                .and_then(|routes| routes.iter().find(|route| *route == "goal-model"))
            else {
                return ResponseTemplate::new(400);
            };
            for task in plan["tasks"].as_array_mut().into_iter().flatten() {
                task["resources"] = json!({"model_route":route});
            }
        }
        plan
    } else if state["trusted_state"]["input"]
        .as_str()
        .is_some_and(|input| input.contains("FIXTURE_NEW_LEFT_RULE"))
    {
        let mut result = json!({"answer":null,"objective":null,"task_updates":[{"task_id":"left","instruction":"Write and verify left.txt as LEFT newline. FIXTURE_NEW_LEFT_RULE: this exact version must be used by the replacement execution."}],"control":null,"permission_response":null,"clarification":null});
        let input = state["trusted_state"]["input"].as_str().unwrap_or_default();
        if input.contains("BAD_RESOURCE_CHANGE_FIXTURE") {
            result["task_updates"][0]["resources"] = json!({"mcp_servers":["unavailable-server"]});
        } else if input.contains("RESOURCE_CHANGE_FIXTURE") {
            let Some(skill) = state["trusted_state"]["available_resources"]["skills"]
                .as_array()
                .and_then(|skills| {
                    skills
                        .iter()
                        .find(|skill| skill["name"] == "goal-resource-fixture")
                })
            else {
                return ResponseTemplate::new(400);
            };
            result["task_updates"][0]["resources"] = json!({"model_route":"goal-model", "mcp_servers":["selected-resource-server"],
                "skills":[{"material_id":skill["material"]["material_id"],"sha256":skill["material"]["sha256"]}],"context_files":["seed.txt"]});
            let Some(context) = state["trusted_state"]["unresolved_inputs"]
                .as_array()
                .and_then(|inputs| {
                    inputs
                        .iter()
                        .find(|input| input["unresolved"]["input_id"] == "invalid-resource-change")
                })
            else {
                return ResponseTemplate::new(400);
            };
            if context["original_input"]["kind"]["text"]
                .as_str()
                .is_none_or(|text| !text.contains("BAD_RESOURCE_CHANGE_FIXTURE"))
            {
                return ResponseTemplate::new(400);
            }
            result["resolutions"] = json!([{"input_id":context["unresolved"]["input_id"], "source_requirements_revision":context["unresolved"]["source_requirements_revision"], "kind":"applied"}]);
        }
        result
    } else if state.get("trusted_state").is_some() {
        json!({"answer":"Fixture explanation: the two actual branches are still active",
            "objective":null,"task_updates":[],"control":null,"permission_response":null,"clarification":null})
    } else if state.get("actual_workspace").is_some() {
        json!({"ready":true,"reason":"Fixture semantic verdict; actual BRO checks remain required","missing_checks":[]})
    } else if state.get("integrated_material").is_some() {
        let supported = state["actual_checks"].as_array().is_some_and(|checks| {
            checks.len() == 3
                && checks.iter().all(|check| {
                    check["actual_output"]["completed"] == true
                        && check["actual_output"]["passed"] == true
                        && check["actual_output"]["timed_out"] == false
                })
        });
        json!({"conditions":[{"id":"delivery","status":if supported { "met" } else { "unverified" },
            "reason":"Fixture judgment over actual integration checks",
            "evidence":["integrated_material","check:has-left","check:has-right","check:has-joined"]}],
            "unmet_original_requirements":[],"constraints_preserved":true,
            "human_acceptance_required":false,"summary":"Fixture delivery; not a real-model quality claim"})
    } else {
        return ResponseTemplate::new(400);
    };
    ResponseTemplate::new(200).set_body_json(json!({"id":"fixture-completion","object":"chat.completion",
        "model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":result.to_string()},
            "finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}))
}
pub fn typed(request: &Request) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let Some(questions) = body["questions"].as_object() else {
        return ResponseTemplate::new(400);
    };
    let modifying = String::from_utf8_lossy(&request.body).contains("FIXTURE_NEW_LEFT_RULE");
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let label = match id.as_str() {
            "query" => "present",
            "modify" if modifying => "present",
            "modify" | "permission_response" => "absent",
            "control" => "none",
            "target" if modifying => "task:left",
            "target" => "goal",
            _ => return ResponseTemplate::new(400),
        };
        let Some(criteria) = question["criteria"].as_object() else {
            return ResponseTemplate::new(400);
        };
        if !criteria.contains_key(label) {
            return ResponseTemplate::new(400);
        }
        let probabilities = criteria
            .keys()
            .map(|key| (key.clone(), json!(if key == label { 1.0 } else { 0.0 })))
            .collect::<serde_json::Map<_, _>>();
        answers.insert(
            id.clone(),
            json!({"type":"choice","choice":label,"confidence":1.0,"probabilities":probabilities}),
        );
    }
    ResponseTemplate::new(200).set_body_json(json!({"model":body["model"],"answers":answers,
        "usage":{"input_tokens":10,"output_tokens":3}}))
}
