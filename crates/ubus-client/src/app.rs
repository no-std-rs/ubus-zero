use serde_json::{json, Value};
use std::{
    io::{self, BufRead, Read, Write},
    os::unix::process::CommandExt,
    process::{Command, ExitCode, Stdio},
    time::{Duration, Instant},
};
use ubus::{
    client::UbusClient,
    envelope::{iter_attrs, try_find, unwrap, MonitorAttr, UbusAttr},
    json::{blobmsg_to_json, json_to_blobmsg},
    server::UbusConnection,
    unix::UnixUbusTransport,
    wire::{Frame, MsgType, MAX_MSG_LEN},
    BlobMsgTable, DispatchOutcome, Transport, UbusError, UbusObject,
};

type Client = UbusClient<UnixUbusTransport>;
const HELP: &str = "ubus-client [OPTIONS] COMMAND [ARGS]

Options:
  -s, --socket PATH     Socket path (UBUS_SOCKET or /var/run/ubus/ubus.sock)
  -t, --timeout SECS    Request timeout; also bounds streams when explicitly set
  -n, --count COUNT     Stop a stream after COUNT messages (default: unlimited)
  -p, --pretty          Pretty-print finite JSON results
  -h, --help            Show this help
  -V, --version         Show the version

Commands:
  list [PATTERN]                  List objects and method signatures as JSON
  call OBJECT METHOD [JSON|-]     Invoke a method; '-' reads JSON from stdin
  send EVENT [JSON|-]             Send an event
  listen [PATTERN...]             Stream matching events as JSON Lines
  subscribe OBJECT...            Stream object notifications as JSON Lines
  monitor                        Stream broker traffic as JSON Lines (requires root)
  wait_for OBJECT...             Wait until all named objects exist
  ping                           Check broker responsiveness
  batch                          Read JSON Lines requests from stdin
  serve OBJECT METHOD... -- COMMAND [ARGS...]
                                 Provide methods using a program's JSON stdin/stdout

Successful finite commands exit 0. Broker errors use their ubus status (1..255),
timeouts exit 7, usage/input errors exit 64, and other failures exit 1.
Diagnostics go to stderr. Streams flush each JSON line immediately.";

struct Options {
    socket: String,
    timeout: Duration,
    stream_timeout: bool,
    count: usize,
    pretty: bool,
}

fn usage(message: impl Into<String>) -> UbusError {
    UbusError::Method(message.into())
}

pub fn main() -> ExitCode {
    let result = run(std::env::args().skip(1).collect());
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ubus-client: {error}");
            ExitCode::from(exit_status(&error))
        }
    }
}

fn exit_status(error: &UbusError) -> u8 {
    match error {
        UbusError::Status(status) if (1..=255).contains(status) => *status as u8,
        UbusError::Timeout(_) => 7,
        UbusError::Method(_) => 64,
        _ => 1,
    }
}

fn run(args: Vec<String>) -> Result<(), UbusError> {
    let mut options = Options {
        socket: std::env::var("UBUS_SOCKET").unwrap_or_else(|_| "/var/run/ubus/ubus.sock".into()),
        timeout: Duration::from_secs(5),
        stream_timeout: false,
        count: 0,
        pretty: false,
    };
    let mut args = args.into_iter().peekable();
    while args.peek().is_some_and(|arg| arg.starts_with('-')) {
        let option = args.next().expect("peeked option");
        match option.as_str() {
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("ubus-client {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--pretty" | "-p" => options.pretty = true,
            "--socket" | "-s" => {
                options.socket = args.next().ok_or_else(|| usage("--socket needs a path"))?
            }
            "--timeout" | "-t" => {
                let value = args
                    .next()
                    .ok_or_else(|| usage("--timeout needs seconds"))?;
                let seconds: f64 = value.parse().map_err(|_| usage("invalid timeout"))?;
                if !seconds.is_finite() || !(0.0..=86400.0).contains(&seconds) || seconds == 0.0 {
                    return Err(usage(
                        "timeout must be greater than zero and at most 86400 seconds",
                    ));
                }
                options.timeout = Duration::from_secs_f64(seconds);
                options.stream_timeout = true;
            }
            "--count" | "-n" => {
                options.count = args
                    .next()
                    .ok_or_else(|| usage("--count needs a number"))?
                    .parse()
                    .map_err(|_| usage("invalid count"))?;
            }
            "--" => break,
            _ => return Err(usage(format!("unknown option {option}; use --help"))),
        }
    }
    let command = args
        .next()
        .ok_or_else(|| usage("a command is required; use --help"))?;
    let args: Vec<String> = args.collect();
    validate_command(&command, &args)?;
    if command == "serve" {
        return serve(&options, &args);
    }
    let mut client = connect(&options)?;
    match command.as_str() {
        "batch" => batch(&options, &mut client),
        "listen" | "subscribe" | "monitor" => stream(&options, &mut client, &command, &args),
        _ => {
            for value in finite(&options, &mut client, &command, &args, None)? {
                output(&value, options.pretty)?;
            }
            Ok(())
        }
    }
}

fn validate_command(command: &str, args: &[String]) -> Result<(), UbusError> {
    let valid = match command {
        "list" => args.len() <= 1,
        "call" => (2..=3).contains(&args.len()),
        "send" => (1..=2).contains(&args.len()),
        "listen" => true,
        "subscribe" | "wait_for" => !args.is_empty(),
        "monitor" | "ping" | "batch" => args.is_empty(),
        "serve" => args
            .iter()
            .position(|s| s == "--")
            .is_some_and(|split| split >= 2 && split + 1 < args.len()),
        _ => return Err(usage(format!("unknown command {command}; use --help"))),
    };
    if valid {
        Ok(())
    } else {
        Err(usage(format!(
            "invalid arguments for {command}; use --help"
        )))
    }
}

fn connect(options: &Options) -> Result<Client, UbusError> {
    let mut transport = UnixUbusTransport::connect(&options.socket)?;
    transport.set_deadline(Some(options.timeout));
    UbusClient::connect(transport)
}

fn payload(argument: Option<&str>) -> Result<BlobMsgTable, UbusError> {
    let text = match argument {
        None => "{}".into(),
        Some("-") => {
            let mut bytes = Vec::new();
            io::stdin()
                .take(MAX_MSG_LEN as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io_error)?;
            if bytes.len() > MAX_MSG_LEN {
                return Err(usage("JSON input exceeds maximum message length"));
            }
            String::from_utf8(bytes).map_err(|_| usage("stdin must be UTF-8 JSON"))?
        }
        Some(text) => text.into(),
    };
    let value: Value =
        serde_json::from_str(&text).map_err(|e| usage(format!("invalid JSON: {e}")))?;
    json_to_blobmsg(&value)
}

fn resolve(client: &mut Client, name: &str) -> Result<u32, UbusError> {
    client
        .lookup_objects(Some(name))?
        .into_iter()
        .find(|object| object.path == name)
        .map(|object| object.id)
        .ok_or(UbusError::Status(4))
}

fn finite(
    options: &Options,
    client: &mut Client,
    command: &str,
    args: &[String],
    data: Option<&Value>,
) -> Result<Vec<Value>, UbusError> {
    client.transport_mut().set_deadline(Some(options.timeout));
    match command {
        "list" => {
            let objects = client.lookup_objects(args.first().map(String::as_str))?;
            let values: Result<Vec<_>, UbusError> = objects.into_iter().map(|object| {
                Ok(json!({"id": object.id, "path": object.path, "methods": blobmsg_to_json(&object.signature)?}))
            }).collect();
            Ok(vec![Value::Array(values?)])
        }
        "call" => {
            let data = match data {
                Some(data) => json_to_blobmsg(data)?,
                None => payload(args.get(2).map(String::as_str))?,
            };
            let id = resolve(client, &args[0])?;
            client
                .invoke_many(id, &args[1], &data)?
                .iter()
                .map(|bytes| blobmsg_to_json(bytes))
                .collect()
        }
        "send" => {
            let data = match data {
                Some(data) => json_to_blobmsg(data)?,
                None => payload(args.get(1).map(String::as_str))?,
            };
            client.send_event(&args[0], &data)?;
            Ok(vec![json!({"ok": true})])
        }
        "ping" => {
            client.ping()?;
            Ok(vec![json!({"ok": true})])
        }
        "wait_for" => {
            let deadline = Instant::now() + options.timeout;
            let mut missing = args.to_vec();
            while !missing.is_empty() {
                if Instant::now() >= deadline {
                    return Err(UbusError::Timeout("wait_for"));
                }
                for name in missing.clone() {
                    match resolve(client, &name) {
                        Ok(_) => missing.retain(|candidate| candidate != &name),
                        Err(UbusError::Status(4)) => {}
                        Err(error) => return Err(error),
                    }
                }
                if !missing.is_empty() {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Ok(vec![json!({"ok": true, "objects": args})])
        }
        _ => Err(usage("batch supports list, call, send, ping and wait_for")),
    }
}

fn output(value: &Value, pretty: bool) -> Result<(), UbusError> {
    let mut stdout = io::stdout().lock();
    if pretty {
        serde_json::to_writer_pretty(&mut stdout, value)
    } else {
        serde_json::to_writer(&mut stdout, value)
    }
    .map_err(|e| UbusError::Io(e.to_string()))?;
    writeln!(stdout).map_err(io_error)?;
    stdout.flush().map_err(io_error)
}

fn stream(
    options: &Options,
    client: &mut Client,
    command: &str,
    args: &[String],
) -> Result<(), UbusError> {
    if command == "monitor" {
        client.monitor(true)?;
    } else {
        let object = client.add_anonymous_object()?;
        if command == "listen" {
            let patterns = if args.is_empty() {
                vec!["*".into()]
            } else {
                args.to_vec()
            };
            for pattern in patterns {
                client.register_event(object, &pattern)?;
            }
        } else {
            for name in args {
                let id = resolve(client, name)?;
                client.subscribe(object, id)?;
            }
        }
    }
    client
        .transport_mut()
        .set_deadline(options.stream_timeout.then_some(options.timeout));
    let mut count = 0;
    loop {
        let Some(frame) = client.recv_frame()? else {
            client.transport_mut().wait_recv();
            continue;
        };
        let value = if command == "monitor" && frame.hdr.ty == MsgType::Monitor {
            monitor_json(&frame)?
        } else if matches!(frame.hdr.ty, MsgType::Invoke | MsgType::Notify) {
            let method = try_find(&frame.payload, UbusAttr::Method)?
                .and_then(|a| a.as_str())
                .ok_or(UbusError::Malformed("stream message missing method"))?;
            let data = try_find(&frame.payload, UbusAttr::Data)?
                .map(|a| a.payload)
                .unwrap_or_default();
            let value = json!({"event": method, "object": try_find(&frame.payload, UbusAttr::ObjId)?.and_then(|a| a.as_u32()), "data": blobmsg_to_json(data)?});
            client.acknowledge(&frame, 0)?;
            value
        } else {
            continue;
        };
        output(&value, false)?;
        count += 1;
        if options.count != 0 && count >= options.count {
            return Ok(());
        }
    }
}

fn monitor_json(frame: &Frame) -> Result<Value, UbusError> {
    let mut value = serde_json::Map::new();
    let mut attrs = iter_attrs(unwrap(&frame.payload)?);
    while let Some(attr) = attrs.try_next()? {
        let (name, field) = match attr.id {
            id if id == MonitorAttr::Client as u8 => ("client", json!(attr.as_u32())),
            id if id == MonitorAttr::Peer as u8 => ("peer", json!(attr.as_u32())),
            id if id == MonitorAttr::Seq as u8 => ("seq", json!(attr.as_u32())),
            id if id == MonitorAttr::Type as u8 => ("type", json!(attr.as_u32())),
            id if id == MonitorAttr::Send as u8 => ("send", json!(attr.as_bool_int8())),
            id if id == MonitorAttr::Data as u8 => {
                let hex: String = attr
                    .payload
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                ("payload_hex", json!(hex))
            }
            _ => continue,
        };
        value.insert(name.into(), field);
    }
    Ok(Value::Object(value))
}

fn batch(options: &Options, client: &mut Client) -> Result<(), UbusError> {
    let mut stdin = io::stdin().lock();
    let mut failed = false;
    loop {
        let mut line = String::new();
        let read = (&mut stdin)
            .take(MAX_MSG_LEN as u64 + 1)
            .read_line(&mut line)
            .map_err(io_error)?;
        if read == 0 {
            break;
        }
        if read > MAX_MSG_LEN {
            return Err(usage("batch request exceeds maximum length"));
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Value =
            serde_json::from_str(&line).map_err(|e| usage(format!("invalid batch JSON: {e}")))?;
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let result = batch_request(options, client, &request);
        match result {
            Ok(values) => output(&json!({"id": id, "ok": true, "result": values}), false)?,
            Err(error) => {
                failed = true;
                output(
                    &json!({"id": id, "ok": false, "error": error.to_string(), "status": exit_status(&error)}),
                    false,
                )?;
            }
        }
    }
    if failed {
        Err(UbusError::Status(1))
    } else {
        Ok(())
    }
}

fn batch_request(
    options: &Options,
    client: &mut Client,
    request: &Value,
) -> Result<Vec<Value>, UbusError> {
    let command = request
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| usage("batch request needs command"))?;
    let field = |name: &str| {
        request
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| usage(format!("batch {command} needs {name}")))
    };
    let args = match command {
        "call" => vec![field("object")?, field("method")?],
        "send" => vec![field("event")?],
        "list" => request
            .get("pattern")
            .and_then(Value::as_str)
            .map(|s| vec![s.into()])
            .unwrap_or_default(),
        "ping" => vec![],
        "wait_for" => request
            .get("objects")
            .and_then(Value::as_array)
            .ok_or_else(|| usage("wait_for needs an objects array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| usage("object names must be strings"))
            })
            .collect::<Result<_, _>>()?,
        _ => return Err(usage("batch supports list, call, send, ping and wait_for")),
    };
    validate_command(command, &args)?;
    finite(options, client, command, &args, request.get("args"))
}

fn serve(options: &Options, args: &[String]) -> Result<(), UbusError> {
    let split = args
        .iter()
        .position(|s| s == "--")
        .expect("validated serve arguments");
    let mut object = UbusObject::new(&args[0]);
    for method in &args[1..split] {
        let program = args[split + 1..].to_vec();
        let method_name = method.clone();
        let object_name = args[0].clone();
        let timeout = options.timeout;
        object = object.method(method, move |data| {
            let value = blobmsg_to_json(data)?;
            let reply = run_handler(&program, &object_name, &method_name, &value, timeout)?;
            json_to_blobmsg(&reply)
        });
    }
    let mut transport = UnixUbusTransport::connect(&options.socket)?;
    transport.set_deadline(Some(options.timeout));
    let mut connection = UbusConnection::connect_and_register(transport, object)?;
    connection.transport_mut().set_deadline(None);
    let mut count = 0;
    loop {
        let outcome = connection.poll_one()?;
        if let DispatchOutcome::HandlerErr(ref error) = outcome {
            eprintln!("ubus-client: handler failed: {error}");
        }
        match outcome {
            DispatchOutcome::Idle => connection.transport_mut().wait_recv(),
            DispatchOutcome::Handled | DispatchOutcome::HandlerErr(_) => {
                count += 1;
                if options.count != 0 && count >= options.count {
                    connection
                        .transport_mut()
                        .set_deadline(Some(options.timeout));
                    return connection.unregister_and_wait();
                }
            }
            _ => {}
        }
    }
}

fn run_handler(
    program: &[String],
    object: &str,
    method: &str,
    value: &Value,
    timeout: Duration,
) -> Result<Value, UbusError> {
    let mut child = Command::new(&program[0])
        .args(&program[1..])
        .process_group(0)
        .env("UBUS_OBJECT", object)
        .env("UBUS_METHOD", method)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(io_error)?;
    let mut input = serde_json::to_vec(value).map_err(|e| usage(e.to_string()))?;
    input.push(b'\n');
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_MSG_LEN as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(io_error)? {
            break Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Err(UbusError::Timeout("handler"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Kill any remaining descendants before joining the pipe readers. A shell
    // may exit while a background child still holds stdout or stdin open.
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let status = status?;
    writer
        .join()
        .map_err(|_| usage("handler input thread failed"))?
        .map_err(io_error)?;
    let bytes = reader
        .join()
        .map_err(|_| usage("handler output thread failed"))?
        .map_err(io_error)?;
    if !status.success() {
        return Err(usage("handler exited unsuccessfully"));
    }
    if bytes.len() > MAX_MSG_LEN {
        return Err(usage("handler output exceeds maximum length"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| usage(format!("handler output must be a JSON object: {e}")))
}

fn io_error(error: io::Error) -> UbusError {
    UbusError::Io(error.to_string())
}
