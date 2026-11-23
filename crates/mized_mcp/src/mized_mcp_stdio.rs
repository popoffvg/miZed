#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    unix::run_mcp_cli()
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsStr,
        fs,
        io::{self, BufRead as _, BufReader, Read as _, Write as _},
        os::unix::{ffi::OsStrExt as _, net::UnixStream},
        path::Path,
        sync::Arc,
        thread,
    };

    use anyhow::{Context as _, Result, bail, ensure};
    use context_server::{
        client::{
            AnyRequest, INTERNAL_ERROR, RequestId, RpcResponse, encode_notification, encode_request,
        },
        protocol::{check_protocol_version, initialize_parameters},
        types::{
            Implementation, InitializeResponse, Notification as _, Request as _,
            notifications::Initialized,
            requests::{CallTool, Initialize, ListTools},
        },
    };
    use serde::{Serialize, de::DeserializeOwned};
    use serde_json::{Value, json};

    const USAGE: &str = "Usage: mized-mcp ADDRESS_FILE [list | call TOOL [JSON | -]]\n\
        Pass - to read the tool arguments from stdin.\n\
        Pass only ADDRESS_FILE to start the MCP stdio bridge.";

    pub(super) fn run_mcp_cli() -> Result<()> {
        let mut arguments = std::env::args_os().skip(1);
        let address_file = arguments.next().context(USAGE)?;
        let command = arguments.next();
        if address_file == "--help" || command.as_ref().is_some_and(|command| command == "--help") {
            writeln!(io::stdout().lock(), "{USAGE}")?;
            return Ok(());
        }
        let request = match command {
            None => None,
            Some(command) if command == "list" && arguments.next().is_none() => {
                Some((ListTools::METHOD, json!({})))
            }
            Some(command) if command == "call" => {
                let name = arguments.next().context(USAGE)?;
                let name = name.to_str().context("Use a UTF-8 miZed MCP tool name")?;
                let input = arguments.next();
                ensure!(arguments.next().is_none(), "{USAGE}");
                let input = match input {
                    None => b"{}".to_vec(),
                    Some(input) if input == "-" => {
                        let mut input = Vec::new();
                        io::stdin()
                            .lock()
                            .read_to_end(&mut input)
                            .context("Could not read miZed MCP tool arguments from stdin")?;
                        input
                    }
                    Some(input) => input.into_encoded_bytes(),
                };
                let input: Value = serde_json::from_slice(&input)
                    .context("Could not parse miZed MCP tool arguments")?;
                ensure!(
                    input.is_object(),
                    "Pass miZed MCP tool arguments as a JSON object"
                );
                Some((
                    CallTool::METHOD,
                    json!({ "name": name, "arguments": input }),
                ))
            }
            _ => bail!("{USAGE}"),
        };

        match request {
            Some((method, parameters)) => {
                let socket = connect_mcp_socket(Path::new(&address_file))?;
                run_mcp_request(socket, method, parameters)
            }
            None => {
                let address_file = Path::new(&address_file).to_path_buf();
                bridge_mcp_stdio(move || connect_mcp_socket(&address_file))
            }
        }
    }

    fn connect_mcp_socket(address_file: &Path) -> Result<UnixStream> {
        let address = fs::read(address_file).with_context(|| {
            format!(
                "Could not read the miZed MCP address from {}",
                address_file.display()
            )
        })?;
        let socket_path = Path::new(OsStr::from_bytes(&address));
        UnixStream::connect(socket_path).with_context(|| {
            format!(
                "Could not connect to miZed MCP at {}",
                socket_path.display()
            )
        })
    }

    fn bridge_mcp_stdio(
        connect: impl Fn() -> Result<UnixStream> + Send + Sync + 'static,
    ) -> Result<()> {
        let connect = Arc::new(connect);
        for line in io::stdin().lock().lines() {
            let line = line.context("Could not read miZed MCP input")?;
            let Ok(request) = serde_json::from_str::<AnyRequest<'_>>(&line) else {
                continue;
            };
            let initialize = request.method == Initialize::METHOD;
            let request_id = request.id;
            let worker_request_id = request_id.clone();
            let connect = connect.clone();
            if let Err(error) = thread::Builder::new().spawn(move || {
                let response =
                    forward_mcp_request(&*connect, &line, &worker_request_id, initialize);
                if let Err(error) = write_bridge_response(&worker_request_id, response) {
                    eprintln!("Could not send miZed MCP response: {error}");
                }
            }) {
                write_bridge_response(
                    &request_id,
                    Err(error).context("Could not start the miZed MCP request"),
                )?;
            }
        }
        Ok(())
    }

    fn forward_mcp_request(
        connect: &impl Fn() -> Result<UnixStream>,
        request: &str,
        request_id: &RequestId,
        initialize: bool,
    ) -> Result<String> {
        let mut socket = BufReader::new(connect()?);
        if !initialize {
            initialize_mcp_socket(&mut socket)?;
        }
        writeln!(socket.get_mut(), "{request}").context("Could not send the miZed MCP request")?;
        read_mcp_response(&mut socket, request_id)
            .context("Could not receive the miZed MCP response")
    }

    fn write_bridge_response(request_id: &RequestId, response: Result<String>) -> Result<()> {
        let response = match response {
            Ok(response) => response,
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": { "code": INTERNAL_ERROR, "message": format!("{error:#}") },
            })
            .to_string(),
        };
        writeln!(io::stdout().lock(), "{response}")?;
        Ok(())
    }

    fn initialize_mcp_socket(socket: &mut BufReader<UnixStream>) -> Result<()> {
        let response: InitializeResponse = read_mcp_result(
            socket,
            1,
            Initialize::METHOD,
            initialize_parameters(Implementation {
                name: "mized-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                description: None,
            }),
        )?;
        check_protocol_version(&response.protocol_version)?;
        writeln!(
            socket.get_mut(),
            "{}",
            encode_notification(Initialized::METHOD, ())?
        )?;
        Ok(())
    }

    fn run_mcp_request(socket: UnixStream, method: &str, parameters: Value) -> Result<()> {
        let mut socket = BufReader::new(socket);
        initialize_mcp_socket(&mut socket)?;
        let result: Value = read_mcp_result(&mut socket, 2, method, parameters)?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!("miZed MCP tool failed: {}", result["content"]);
        }
        let mut output = io::stdout().lock();
        serde_json::to_writer(
            &mut output,
            result.get("structuredContent").unwrap_or(&result),
        )?;
        writeln!(output)?;
        Ok(())
    }

    fn read_mcp_result<T: DeserializeOwned>(
        socket: &mut BufReader<UnixStream>,
        id: i32,
        method: &str,
        parameters: impl Serialize,
    ) -> Result<T> {
        let request_id = RequestId::Int(id);
        let request = encode_request(request_id.clone(), method, parameters)?;
        writeln!(socket.get_mut(), "{request}")?;
        let response = read_mcp_response(socket, &request_id)
            .with_context(|| format!("Could not receive the miZed MCP response for {method}"))?;
        serde_json::from_str::<RpcResponse<'_>>(&response)?
            .result()
            .with_context(|| format!("miZed MCP request {method} failed"))
    }

    fn read_mcp_response(
        socket: &mut BufReader<UnixStream>,
        request_id: &RequestId,
    ) -> Result<String> {
        let mut line = String::new();
        loop {
            line.clear();
            ensure!(
                socket.read_line(&mut line)? != 0,
                "The miZed MCP socket closed"
            );
            if let Ok(response) = serde_json::from_str::<RpcResponse>(&line)
                && response.request_id() == request_id
            {
                line.truncate(line.trim_end_matches(['\r', '\n']).len());
                return Ok(line);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{cell::RefCell, net::Shutdown};

        use context_server::types::LATEST_PROTOCOL_VERSION;

        use super::*;

        #[test]
        fn next_request_connects_after_editor_disconnect() -> Result<()> {
            let (first_socket, mut first_editor) = UnixStream::pair()?;
            let (next_socket, mut next_editor) = UnixStream::pair()?;
            for editor in [&mut first_editor, &mut next_editor] {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": LATEST_PROTOCOL_VERSION,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "miZed", "version": "0" },
                    },
                });
                writeln!(editor, "{reply}")?;
            }
            first_editor.shutdown(Shutdown::Write)?;
            let next_reply = json!({"jsonrpc": "2.0", "id": "list", "result": {"tools": []}});
            writeln!(next_editor, "{next_reply}")?;

            let sockets = RefCell::new([first_socket, next_socket].into_iter());
            let connect = || {
                sockets
                    .borrow_mut()
                    .next()
                    .context("No test socket is available")
            };
            let first_id = RequestId::Str("add".into());
            let first_request = encode_request(
                first_id.clone(),
                CallTool::METHOD,
                json!({"name": "add_comment", "arguments": {}}),
            )?;
            let interrupted = forward_mcp_request(&connect, &first_request, &first_id, false);
            let Err(error) = interrupted else {
                bail!("Expected the first miZed MCP request to fail");
            };
            assert_eq!(
                error.root_cause().to_string(),
                "The miZed MCP socket closed"
            );

            let next_id = RequestId::Str("list".into());
            let next_request = encode_request(next_id.clone(), ListTools::METHOD, json!({}))?;
            let reply = forward_mcp_request(&connect, &next_request, &next_id, false)?;
            assert_eq!(serde_json::from_str::<Value>(&reply)?, next_reply);

            for (mut editor, expected_request) in
                [(first_editor, first_request), (next_editor, next_request)]
            {
                let mut requests = String::new();
                editor.read_to_string(&mut requests)?;
                let mut requests = requests.lines();
                let initialize: Value = serde_json::from_str(
                    requests.next().context("Expected an initialize request")?,
                )?;
                assert_eq!(initialize["method"], Initialize::METHOD);
                let initialized: Value = serde_json::from_str(
                    requests
                        .next()
                        .context("Expected an initialized notification")?,
                )?;
                assert_eq!(initialized["method"], Initialized::METHOD);
                assert_eq!(requests.next(), Some(expected_request.as_str()));
                assert_eq!(requests.next(), None);
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("miZed MCP requires a Unix socket");
    std::process::ExitCode::FAILURE
}
