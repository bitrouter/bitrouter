//! Native Responses fixture for workers whose tools cannot convert to Chat.

use super::*;

fn functions(tools: &[Value], namespace: Option<&str>) -> Vec<(Value, Option<String>)> {
    tools
        .iter()
        .flat_map(|tool| {
            if tool["type"] == "namespace" {
                return tool["tools"]
                    .as_array()
                    .map(|children| functions(children, tool["name"].as_str()))
                    .unwrap_or_default();
            }
            if (tool["type"] == "function" || tool["type"] == "custom") && tool["name"].is_string()
            {
                vec![(
                    json!({"function":{"name":tool["name"]},"type":tool["type"]}),
                    namespace.map(str::to_owned),
                )]
            } else {
                Vec::new()
            }
        })
        .collect()
}

pub(super) fn reply(request: &Request, coding: bool) -> Result<ResponseTemplate> {
    let body: Value = serde_json::from_slice(&request.body)?;
    let model = body["model"].as_str().context("Responses model missing")?;
    let available = body["tools"]
        .as_array()
        .map(|tools| functions(tools, None))
        .unwrap_or_default();
    let tool_result = body["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["type"] == "function_call_output" || item["type"] == "custom_tool_call_output"
        })
    });
    let shell = available.iter().any(|(tool, _)| {
        matches!(
            tool["function"]["name"].as_str(),
            Some("exec_command" | "Bash" | "shell_command" | "exec")
        )
    });
    let output = if coding
        && body.to_string().contains("NATIVE_CODE_EXERCISE")
        && !tool_result
        && shell
    {
        let custom_exec = available
            .iter()
            .find(|(tool, _)| tool["type"] == "custom" && tool["function"]["name"] == "exec");
        if let Some((_, namespace)) = custom_exec {
            let command = coding::command()?;
            let command = if body.to_string().contains("NATIVE_FAILING_IMPLEMENTATION") {
                command.replace("a + b", "a - b")
            } else {
                command
            };
            let arguments = json!({"cmd":command,"yield_time_ms":10000,"max_output_tokens":2000});
            let mut item = json!({"id":"ct-native-coding","type":"custom_tool_call","status":"completed",
                "call_id":"native-code-check","name":"exec",
                "input":format!("const result = await tools.exec_command({arguments}); text(result);")});
            if let Some(namespace) = namespace {
                item["namespace"] = json!(namespace);
            }
            item
        } else {
            let mut compatible = body.clone();
            compatible["tools"] = json!(available.iter().map(|(tool, _)| tool).collect::<Vec<_>>());
            let call = coding::tool_call(&compatible)?;
            let name = call["function"]["name"]
                .as_str()
                .context("shell tool name missing")?;
            let mut item = json!({"id":"fc-native-coding","type":"function_call","status":"completed",
            "call_id":"native-code-check","name":name,"arguments":call["function"]["arguments"]});
            if let Some((_, Some(namespace))) = available
                .iter()
                .find(|(tool, _)| tool["function"]["name"] == name)
            {
                item["namespace"] = json!(namespace);
            }
            item
        }
    } else {
        ensure!(
            !coding || tool_result || !body.to_string().contains("NATIVE_CODE_EXERCISE"),
            "coding worker omitted a recognized shell tool: {available:?}"
        );
        let text = if body
            .to_string()
            .contains("Controlled follow-up after the trial.")
        {
            "NATIVE_BASELINE_OK"
        } else if coding {
            "NATIVE_CODE_DONE"
        } else {
            "NATIVE_TERMINAL_OK"
        };
        json!({"id":"msg-native-response","type":"message","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":text,"annotations":[]}]})
    };
    let completed = json!({"id":"resp-native-response","object":"response","created_at":0,
        "status":"completed","model":model,"output":[output],
        "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}});
    let response = if body["stream"] == true {
        let mut added = output.clone();
        added["status"] = json!("in_progress");
        let mut events = vec![
            json!({"type":"response.created","sequence_number":0,"response":{
                "id":"resp-native-response","object":"response","created_at":0,
                "status":"in_progress","model":model,"output":[]}}),
        ];
        if output["type"] == "custom_tool_call" {
            added["input"] = json!("");
            events.push(json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}));
            events.push(
                json!({"type":"response.custom_tool_call_input.delta","sequence_number":2,
                "item_id":output["id"],"output_index":0,"delta":output["input"]}),
            );
            events.push(
                json!({"type":"response.custom_tool_call_input.done","sequence_number":3,
                "item_id":output["id"],"output_index":0,"input":output["input"]}),
            );
        } else if output["type"] == "function_call" {
            added["arguments"] = json!("");
            events.push(json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}));
            events.push(
                json!({"type":"response.function_call_arguments.delta","sequence_number":2,
                "item_id":output["id"],"output_index":0,"delta":output["arguments"]}),
            );
            events.push(
                json!({"type":"response.function_call_arguments.done","sequence_number":3,
                "item_id":output["id"],"output_index":0,"arguments":output["arguments"]}),
            );
        } else {
            added["content"] = json!([]);
            let text = &output["content"][0]["text"];
            events.push(json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":added}));
            events.push(
                json!({"type":"response.content_part.added","sequence_number":2,
                "item_id":output["id"],"output_index":0,"content_index":0,
                "part":{"type":"output_text","text":"","annotations":[]}}),
            );
            events.push(
                json!({"type":"response.output_text.delta","sequence_number":3,
                "item_id":output["id"],"output_index":0,"content_index":0,"delta":text}),
            );
            events.push(
                json!({"type":"response.output_text.done","sequence_number":4,
                "item_id":output["id"],"output_index":0,"content_index":0,"text":text}),
            );
            events.push(json!({"type":"response.content_part.done","sequence_number":5,
                "item_id":output["id"],"output_index":0,"content_index":0,"part":output["content"][0]}));
        }
        events.push(json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":output}));
        events.push(json!({"type":"response.completed","sequence_number":7,"response":completed}));
        let stream = events
            .iter()
            .map(|event| {
                let kind = event["type"].as_str().unwrap_or("response.invalid");
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stream)
    } else {
        ResponseTemplate::new(200).set_body_json(completed)
    };
    Ok(
        response.set_delay(Duration::from_millis(if model == "strong" {
            250
        } else {
            5
        })),
    )
}
