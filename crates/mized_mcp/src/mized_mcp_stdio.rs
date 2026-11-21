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
        thread,
    };

    use anyhow::{Context as _, Result, bail, ensure};
    use context_server::{
        client::{RequestId, RpcResponse, encode_notification, encode_request},
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

        let socket = connect_mcp_socket(Path::new(&address_file))?;
        match request {
            Some((method, parameters)) => run_mcp_request(socket, method, parameters),
            None => bridge_mcp_stdio(socket),
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

    fn bridge_mcp_stdio(mut output: UnixStream) -> Result<()> {
        let mut input = output.try_clone()?;
        thread::spawn(move || {
            if let Err(error) = io::copy(&mut io::stdin().lock(), &mut input)
                .and_then(|_| input.shutdown(std::net::Shutdown::Write))
            {
                eprintln!("Could not send miZed MCP input: {error}");
            }
        });
        io::copy(&mut output, &mut io::stdout().lock())
            .context("Could not read miZed MCP output")?;
        Ok(())
    }

    fn run_mcp_request(socket: UnixStream, method: &str, parameters: Value) -> Result<()> {
        let mut socket = BufReader::new(socket);
        let response: InitializeResponse = read_mcp_result(
            &mut socket,
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
        let mut line = String::new();
        loop {
            line.clear();
            ensure!(
                socket.read_line(&mut line)? != 0,
                "miZed MCP disconnected during {method}"
            );
            if let Ok(response) = serde_json::from_str::<RpcResponse>(&line)
                && response.request_id() == &request_id
            {
                return response
                    .result()
                    .with_context(|| format!("miZed MCP request {method} failed"));
            }
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("miZed MCP requires a Unix socket");
    std::process::ExitCode::FAILURE
}
