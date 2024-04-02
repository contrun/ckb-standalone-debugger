use ckb_chain_spec::consensus::ConsensusBuilder;
use ckb_debugger_api::embed::Embed;
use ckb_debugger_api::DummyResourceLoader;
use ckb_debugger_api::{check, get_script_hash_by_index};
use ckb_mock_tx_types::{MockTransaction, ReprMockTransaction, Resource};
use ckb_script::cost_model::transferred_byte_cycles;
use ckb_script::{
    update_caller_machine, MachineContext, ResumableMachine, ScriptGroupType, ScriptVersion,
    TransactionScriptsVerifier, TxVerifyEnv,
};
use ckb_types::core::cell::resolve_transaction;
use ckb_types::core::HeaderView;
use ckb_types::packed::Byte32;
use ckb_types::prelude::Entity;
use ckb_vm::cost_model::estimate_cycles;
use ckb_vm::decoder::build_decoder;
use ckb_vm::error::Error;
use ckb_vm::instructions::instruction_length;
use ckb_vm::instructions::insts::OP_ECALL;
use ckb_vm::instructions::tagged::TaggedInstruction;
use ckb_vm::instructions::{extract_opcode, instruction_opcode_name};
use ckb_vm::memory::flat::FlatMemory;
use ckb_vm::registers::A7;
use ckb_vm::{
    Bytes, CoreMachine, DefaultCoreMachine, DefaultMachine, DefaultMachineBuilder, Instruction, Register,
    SupportMachine, WXorXMemory,
};
use ckb_vm_debug_utils::ElfDumper;
#[cfg(feature = "stdio")]
use ckb_vm_debug_utils::Stdio;
use ckb_vm_pprof::{PProfMachine, Profile};
use clap::{crate_version, App, Arg};
use serde::{Deserialize, Serialize};
use serde_json::from_str as from_json_str;
use serde_json::to_writer;
use serde_plain::from_str as from_plain_str;
use std::fs::{read, read_to_string};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{collections::HashSet, io::Read};
mod decode;
mod misc;
use decode::decode_instruction;
use misc::{FileOperation, FileStream, HumanReadableCycles, Random, TimeNow};

#[cfg(feature = "probes")]
type MemoryType = ckb_vm::FlatMemory<u64>;
#[cfg(not(feature = "probes"))]
type MemoryType = ckb_vm::SparseMemory<u64>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    drop(env_logger::init());

    let default_max_cycles = format!("{}", 70_000_000u64);
    let default_script_version = "2";
    let default_mode = "full";
    let default_gdb_specify_depth = "0";

    let matches = App::new("ckb-debugger")
        .version(crate_version!())
        .arg(
            Arg::with_name("bin")
                .long("bin")
                .help("File used to replace the binary denoted in the script")
                .takes_value(true),
        )
        .arg(Arg::with_name("cell-index").long("cell-index").short("i").help("Index of cell to run").takes_value(true))
        .arg(
            Arg::with_name("cell-type")
                .long("cell-type")
                .short("t")
                .possible_values(&["input", "output"])
                .help("Type of cell to run")
                .takes_value(true),
        )
        .arg(Arg::with_name("dump-file").long("dump-file").help("Dump file name").takes_value(true))
        .arg(
            Arg::with_name("gdb-listen")
                .long("gdb-listen")
                .help("Address to listen for GDB remote debugging server")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("gdb-specify-depth")
                .long("gdb-specify-depth")
                .help("Specifies the depth of the exec/spawn stack")
                .default_value(&default_gdb_specify_depth)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("max-cycles")
                .long("max-cycles")
                .default_value(&default_max_cycles)
                .help("Max cycles")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("mode")
                .long("mode")
                .help("Execution mode of debugger")
                .possible_values(&["full", "fast", "gdb", "probe", "gdb_gdbstub", "trace_dump"])
                .default_value(&default_mode)
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("pprof")
                .long("pprof")
                .help("Performance profiling, specify output file for further use")
                .takes_value(true),
        )
        .arg(Arg::with_name("script-hash").long("script-hash").help("Script hash").takes_value(true))
        .arg(
            Arg::with_name("script-group-type")
                .long("script-group-type")
                .short("s")
                .possible_values(&["lock", "type"])
                .help("Script group type")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("script-version")
                .long("script-version")
                .default_value(&default_script_version)
                .help("Script version")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("skip-end")
                .long("skip-end")
                .help("End address to skip printing debug info")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("skip-start")
                .long("skip-start")
                .help("Start address to skip printing debug info")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("step")
                .long("step")
                .multiple(true)
                .help("Set to true to enable step mode, where we print PC address for each instruction"),
        )
        .arg(
            Arg::with_name("prompt")
                .long("prompt")
                .required(false)
                .takes_value(false)
                .help("Set to true to prompt for stdin input before executing"),
        )
        .arg(
            Arg::with_name("tx-file")
                .long("tx-file")
                .short("f")
                .required_unless_one(&["bin", "decode"])
                .help("Filename containing JSON formatted transaction dump")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("trace-file")
                .long("trace-file")
                .help("Filename to which the executation trace dump will be saved to")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("read-file")
                .long("read-file")
                .help("Read content from local file or stdin. Then feed the content to syscall in scripts")
                .takes_value(true),
        )
        .arg(Arg::with_name("args").multiple(true))
        .arg(
            Arg::with_name("decode")
                .long("decode")
                .help("Decode RISC-V instruction")
                .takes_value(true)
                .conflicts_with_all(&["bin", "tx-file"]),
        )
        .arg(
            Arg::with_name("disable-overlapping-detection")
                .long("disable-overlapping-detection")
                .required(false)
                .takes_value(false)
                .help("Set to true to disable overlapping detection between stack and heap"),
        )
        .get_matches();

    let matches_decode = matches.value_of_lossy("decode");
    if matches_decode.is_some() {
        return decode_instruction(&matches_decode.unwrap());
    }

    let matches_bin = matches.value_of("bin");
    let matches_cell_index = matches.value_of("cell-index");
    let matches_cell_type = matches.value_of("cell-type");
    let matches_pprof = matches.value_of("pprof");
    let matches_dump_file = matches.value_of("dump-file");
    let matches_gdb_listen = matches.value_of("gdb-listen");
    let matches_gdb_specify_depth = matches.value_of("gdb-specify-depth").unwrap();
    let matches_max_cycles = matches.value_of("max-cycles").unwrap();
    let matches_mode = matches.value_of("mode").unwrap();
    let matches_script_hash = matches.value_of("script-hash");
    let matches_script_group_type = matches.value_of("script-group-type");
    let matches_script_version = matches.value_of("script-version").unwrap();
    let matches_skip_end = matches.value_of("skip-end");
    let matches_skip_start = matches.value_of("skip-start");
    let matches_step = matches.occurrences_of("step");
    let matches_tx_file = matches.value_of("tx-file");
    let matches_trace_file = matches.value_of("trace-file");
    let matches_args = matches.values_of("args").unwrap_or_default();
    let matches_read_file_name = matches.value_of("read-file");

    let verifier_args: Vec<String> = matches_args.into_iter().map(|s| s.into()).collect();
    let verifier_args_byte: Vec<Bytes> = verifier_args.into_iter().map(|s| s.into()).collect();

    let fs_syscall = if let Some(file_name) = matches_read_file_name {
        Some(FileStream::new(file_name))
    } else {
        None
    };

    let verifier_max_cycles: u64 = matches_max_cycles.parse()?;
    let verifier_mock_tx: MockTransaction = {
        let mock_tx = if matches_tx_file.is_none() {
            String::from_utf8_lossy(include_bytes!("./dummy_tx.json")).to_string()
        } else {
            let matches_tx_file = matches_tx_file.unwrap();
            if matches_tx_file == "-" {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                let mock_tx = read_to_string(matches_tx_file)?;
                let mut mock_tx_embed = Embed::new(PathBuf::from(matches_tx_file.to_string()), mock_tx.clone());
                mock_tx_embed.replace_all()
            }
        };
        let repr_mock_tx: ReprMockTransaction = from_json_str(&mock_tx)?;
        if let Err(msg) = check(&repr_mock_tx) {
            eprintln!("Warning, potential format error found: {}", msg);
            eprintln!("If tx-file is crafted manually, please double check it.")
        }
        repr_mock_tx.into()
    };
    let verifier_script_group_type = {
        let script_group_type = if matches_tx_file.is_none() {
            "type"
        } else {
            matches_script_group_type.unwrap()
        };
        from_plain_str(script_group_type)?
    };
    let verifier_script_hash = if matches_tx_file.is_none() {
        verifier_mock_tx.mock_info.inputs[0].output.calc_lock_hash()
    } else if let Some(hex_script_hash) = matches_script_hash {
        if hex_script_hash.len() != 66 || (!hex_script_hash.starts_with("0x")) {
            panic!("Invalid script hash format!");
        }
        let b = hex::decode(&hex_script_hash.as_bytes()[2..])?;
        Byte32::from_slice(b.as_slice())?
    } else {
        let mut cell_type = matches_cell_type;
        let mut cell_index = matches_cell_index;
        match verifier_script_group_type {
            ScriptGroupType::Lock => {
                if cell_type.is_none() {
                    cell_type = Some("input");
                }
                if cell_index.is_none() {
                    cell_index = Some("0");
                    println!("cell_index is not specified. Assume --cell-index = 0")
                }
            }
            ScriptGroupType::Type => {
                if cell_type.is_none() || cell_index.is_none() {
                    panic!("You must provide either script hash, or cell type + cell index");
                }
            }
        }
        let cell_type = cell_type.unwrap();
        let cell_index: usize = cell_index.unwrap().parse()?;
        get_script_hash_by_index(&verifier_mock_tx, &verifier_script_group_type, cell_type, cell_index)
    };
    let verifier_script_version = match matches_script_version {
        "0" => ScriptVersion::V0,
        "1" => ScriptVersion::V1,
        "2" => ScriptVersion::V2,
        _ => panic!("wrong script version"),
    };
    let verifier_resource = Resource::from_both(&verifier_mock_tx, DummyResourceLoader {})?;
    let verifier_resolve_transaction = resolve_transaction(
        verifier_mock_tx.core_transaction(),
        &mut HashSet::new(),
        &verifier_resource,
        &verifier_resource,
    )?;
    let consensus = Arc::new(ConsensusBuilder::default().build());
    let tx_env = Arc::new(TxVerifyEnv::new_commit(&HeaderView::new_advanced_builder().build()));
    let mut verifier = TransactionScriptsVerifier::new(
        Arc::new(verifier_resolve_transaction),
        verifier_resource,
        consensus.clone(),
        tx_env.clone(),
    );
    verifier.set_debug_printer(Box::new(move |_hash: &Byte32, message: &str| {
        print!("{}", message);
        if !message.ends_with('\n') {
            println!("");
        }
    }));
    let verifier_script_group = verifier.find_script_group(verifier_script_group_type, &verifier_script_hash).unwrap();
    let verifier_program = match matches_bin {
        Some(path) => {
            let data = read(path)?;
            data.into()
        }
        None => verifier.extract_script(&verifier_script_group.script)?,
    };

    let machine_context = Arc::new(Mutex::new(MachineContext::default()));
    let machine_init = || {
        let machine_core = DefaultCoreMachine::<u64, WXorXMemory<MemoryType>>::new(
            verifier_script_version.vm_isa(),
            verifier_script_version.vm_version(),
            verifier_max_cycles,
        );
        #[cfg(feature = "stdio")]
        let mut machine_builder = DefaultMachineBuilder::new(machine_core)
            .instruction_cycle_func(Box::new(estimate_cycles))
            .syscall(Box::new(Stdio::new(false)));
        #[cfg(not(feature = "stdio"))]
        let mut machine_builder =
            DefaultMachineBuilder::new(machine_core).instruction_cycle_func(Box::new(estimate_cycles));
        if let Some(data) = matches_dump_file {
            machine_builder = machine_builder.syscall(Box::new(ElfDumper::new(data.to_string(), 4097, 64)));
        }
        let machine_syscalls =
            verifier.generate_syscalls(verifier_script_version, verifier_script_group, machine_context.clone());
        machine_builder =
            machine_syscalls.into_iter().fold(machine_builder, |builder, syscall| builder.syscall(syscall));
        let machine_builder = if let Some(fs) = fs_syscall.clone() {
            machine_builder.syscall(Box::new(fs))
        } else {
            machine_builder
        };
        let machine_builder = machine_builder.syscall(Box::new(TimeNow::new()));
        let machine_builder = machine_builder.syscall(Box::new(Random::new()));
        let machine_builder = machine_builder.syscall(Box::new(FileOperation::new()));
        let machine = machine_builder.build();
        machine
    };

    let machine_step =
        |machine: &mut PProfMachine<DefaultCoreMachine<u64, WXorXMemory<MemoryType>>>| -> Result<i8, ckb_vm::Error> {
            machine.machine.set_running(true);
            let mut decoder =
                build_decoder::<u64>(verifier_script_version.vm_isa(), verifier_script_version.vm_version());
            let mut step_result = Ok(());
            let skip_range = if let (Some(s), Some(e)) = (matches_skip_start, matches_skip_end) {
                let s = u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("parse skip start");
                let e = u64::from_str_radix(e.trim_start_matches("0x"), 16).expect("parse skip end");
                Some(std::ops::Range { start: s, end: e })
            } else {
                None
            };
            while machine.machine.running() && step_result.is_ok() {
                let mut print_info = true;
                if let Some(skip_range) = &skip_range {
                    if skip_range.contains(machine.machine.pc()) {
                        print_info = false;
                    }
                }
                if print_info {
                    println!("PC: 0x{:x}", machine.machine.pc());
                    if matches_step > 1 {
                        println!("Machine: {}", machine.machine);
                    }
                }
                step_result = machine.machine.step(&mut decoder);
            }
            if step_result.is_err() {
                Err(step_result.unwrap_err())
            } else {
                Ok(machine.machine.exit_code())
            }
        };

    if matches_mode == "full" {
        let mut machine = PProfMachine::new(
            machine_init(),
            Profile::new(&verifier_program)?
                .set_disable_overlapping_detection(matches.is_present("disable-overlapping-detection")),
        );
        let bytes = machine.load_program(&verifier_program, &verifier_args_byte)?;
        let transferred_cycles = transferred_byte_cycles(bytes);
        machine.machine.add_cycles(transferred_cycles)?;
        let result = if matches_step > 0 {
            machine_step(&mut machine)
        } else {
            machine.run()
        };
        match result {
            Ok(data) => {
                println!("Run result: {:?}", data);
                println!(
                    "Total cycles consumed: {}",
                    HumanReadableCycles(machine.machine.cycles())
                );
                println!(
                    "Transfer cycles: {}, running cycles: {}",
                    HumanReadableCycles(transferred_cycles),
                    HumanReadableCycles(machine.machine.cycles() - transferred_cycles)
                );
                if let Some(fp) = matches_pprof {
                    let mut output = std::fs::File::create(&fp)?;
                    machine.profile.display_flamegraph(&mut output);
                }
                if data != 0 {
                    std::process::exit(254);
                }
            }
            Err(err) => {
                println!("Trace:");
                machine.profile.display_stacktrace("  ", &mut std::io::stdout());
                println!("Error:");
                println!("  {:?}", err);
                println!("Machine: {}", machine.machine);
            }
        }
        return Ok(());
    }

    if matches_mode == "fast" {
        let mut machine = machine_init();
        let bytes = machine.load_program(&verifier_program, &verifier_args_byte)?;
        let transferred_cycles = transferred_byte_cycles(bytes);
        machine.add_cycles(transferred_cycles)?;
        let result = machine.run();
        println!("Run result: {:?}", result);
        println!("Total cycles consumed: {}", HumanReadableCycles(machine.cycles()));
        println!(
            "Transfer cycles: {}, running cycles: {}",
            HumanReadableCycles(transferred_cycles),
            HumanReadableCycles(machine.cycles() - transferred_cycles)
        );
        if let Ok(data) = result {
            if data != 0 {
                std::process::exit(254);
            }
        }
        return Ok(());
    }

    if matches_mode == "gdb" || matches_mode == "gdb_gdbstub" {
        let listen_address = matches_gdb_listen.unwrap();
        let listener = TcpListener::bind(listen_address)?;
        println!("Listening for gdb remote connection on {}", listen_address);
        for res in listener.incoming() {
            if let Ok(stream) = res {
                println!("Accepted connection from: {}, booting VM", stream.peer_addr()?);
                let mut machine = machine_init();
                let bytes = machine.load_program(&verifier_program, &verifier_args_byte)?;
                let transferred_cycles = transferred_byte_cycles(bytes);
                machine.add_cycles(transferred_cycles)?;
                machine.set_running(true);

                let specify = usize::from_str_radix(matches_gdb_specify_depth, 10).unwrap();
                let machine = machine_sget(machine, machine_context.clone(), specify)?;

                if matches_mode == "gdb" {
                    let h = ckb_vm_debug_utils::GdbHandler::new(machine);
                    ckb_gdb_remote_protocol::process_packets_from(stream.try_clone().unwrap(), stream, h);
                } else if matches_mode == "gdb_gdbstub" {
                    use ckb_vm_debug_utils::{GdbStubHandler, GdbStubHandlerEventLoop};
                    use gdbstub::{
                        conn::ConnectionExt,
                        stub::{DisconnectReason, GdbStub, GdbStubError},
                    };
                    use gdbstub_arch::riscv::Riscv64;
                    let mut h: GdbStubHandler<_, Riscv64> = GdbStubHandler::new(machine);
                    let connection: Box<(dyn ConnectionExt<Error = std::io::Error> + 'static)> = Box::new(stream);
                    let gdb = GdbStub::new(connection);

                    let result = match gdb.run_blocking::<GdbStubHandlerEventLoop<_, _>>(&mut h) {
                        Ok(disconnect_reason) => match disconnect_reason {
                            DisconnectReason::Disconnect => {
                                println!("GDB client has disconnected. Running to completion...");
                                h.run_till_exited()
                            }
                            DisconnectReason::TargetExited(_) => h.run_till_exited(),
                            DisconnectReason::TargetTerminated(sig) => {
                                Err(Error::External(format!("Target terminated with signal {}!", sig)))
                            }
                            DisconnectReason::Kill => Err(Error::External("GDB sent a kill command!".to_string())),
                        },
                        Err(GdbStubError::TargetError(e)) => {
                            Err(Error::External(format!("target encountered a fatal error: {}", e)))
                        }
                        Err(e) => Err(Error::External(format!("gdbstub encountered a fatal error: {}", e))),
                    };
                    match result {
                        Ok((exit_code, cycles)) => {
                            println!("Exit code: {:?}", exit_code);
                            println!("Total cycles consumed: {}", HumanReadableCycles(cycles));
                            println!(
                                "Transfer cycles: {}, running cycles: {}",
                                HumanReadableCycles(transferred_cycles),
                                HumanReadableCycles(cycles - transferred_cycles)
                            );
                        }
                        Err(e) => {
                            println!("Error: {}", e);
                        }
                    }
                }
            }
        }
        return Ok(());
    }

    if matches_mode == "trace_dump" {
        use ckb_vm::instructions::execute;

        let writer = matches_trace_file.map(|f| std::fs::File::create(f).expect("open trace file"));
        let mut traces = vec![];

        #[derive(Debug, Clone)]
        pub struct ExecutionTrace {
            pub opcode: String,
            pub ckb_vm_instruction: TaggedInstruction,
            pub mnemonics: String,
            pub op_a: u32,
            pub op_b: u32,
            pub op_c: u32,
            pub imm_b: bool,
            pub imm_c: bool,
        }

        let mut machine = machine_init();
        let bytes = machine.load_program(&verifier_program, &verifier_args_byte)?;
        let transferred_cycles = transferred_byte_cycles(bytes);
        machine.add_cycles(transferred_cycles)?;
        let mut global_clk: u64 = 0;

        machine.set_running(true);
        // Hardcode script version to v0 so that we don't use the new instruction set.
        // because we want to start with less instructions.
        let verifier_script_version = ScriptVersion::V0;
        let mut decoder = build_decoder::<u64>(verifier_script_version.vm_isa(), verifier_script_version.vm_version());

        let mut step_result = Ok(());
        while machine.running() && step_result.is_ok() {
            let pc = machine.pc().to_u64();
            step_result = decoder
                .decode(machine.memory_mut(), pc)
                .and_then(|inst| {
                    let cycles = machine.instruction_cycle_func()(inst);
                    machine.add_cycles(cycles).map(|_| inst)
                })
                .and_then(|inst| {
                    let regs = machine.registers().iter().map(|r| r.to_u64()).collect::<Vec<_>>();
                    let tagged_inst = TaggedInstruction::try_from(inst).expect("valid tagged instruction");
                    let cycles = machine.cycles();
                    let trace = TraceItem::new(global_clk, pc, regs, TracedInstruction::new(tagged_inst));
                    dbg!(global_clk, pc, cycles, &trace);
                    if writer.is_some() {
                        traces.push(trace);
                    }
                    let r = execute(inst, &mut machine);
                    global_clk = global_clk + 1;
                    r
                });
        }
        let result = step_result.map(|_| machine.exit_code());

        println!("Run result: {:?}", result);
        println!("Total cycles consumed: {}", HumanReadableCycles(machine.cycles()));
        println!(
            "Transfer cycles: {}, running cycles: {}",
            HumanReadableCycles(transferred_cycles),
            HumanReadableCycles(machine.cycles() - transferred_cycles)
        );

        if let Some(mut writer) = writer {
            to_writer(&mut writer, &traces).expect("write trace file");
        }
    }

    if matches_mode == "probe" {
        #[cfg(not(feature = "probes"))]
        {
            println!("To use probe mode, feature probes must be enabled!");
            return Ok(());
        }

        #[cfg(feature = "probes")]
        {
            use ckb_vm::instructions::execute;
            use probe::probe;
            use std::io::BufRead;

            let prompt = matches.is_present("prompt");
            if prompt {
                println!("Enter to start executing:");
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line).expect("read");
            }

            let mut machine = machine_init();
            let bytes = machine.load_program(&verifier_program, &verifier_args_byte)?;
            let transferred_cycles = transferred_byte_cycles(bytes);
            machine.add_cycles(transferred_cycles)?;

            machine.set_running(true);
            let mut decoder =
                build_decoder::<u64>(verifier_script_version.vm_isa(), verifier_script_version.vm_version());

            let mut step_result = Ok(());
            while machine.running() && step_result.is_ok() {
                let pc = machine.pc().to_u64();
                step_result = decoder
                    .decode(machine.memory_mut(), pc)
                    .and_then(|inst| {
                        let cycles = machine.instruction_cycle_func()(inst);
                        machine.add_cycles(cycles).map(|_| inst)
                    })
                    .and_then(|inst| {
                        let regs = machine.registers().as_ptr();
                        let memory = (&mut machine.memory_mut().inner_mut()).as_ptr();
                        let cycles = machine.cycles();
                        probe!(ckb_vm, execute_inst, pc, cycles, inst, regs, memory);
                        let r = execute(inst, &mut machine);
                        let cycles = machine.cycles();
                        probe!(
                            ckb_vm,
                            execute_inst_end,
                            pc,
                            cycles,
                            inst,
                            regs,
                            memory,
                            if r.is_ok() { 0 } else { 1 }
                        );
                        r
                    });
            }
            let result = step_result.map(|_| machine.exit_code());

            println!("Run result: {:?}", result);
            println!("Total cycles consumed: {}", HumanReadableCycles(machine.cycles()));
            println!(
                "Transfer cycles: {}, running cycles: {}",
                HumanReadableCycles(transferred_cycles),
                HumanReadableCycles(machine.cycles() - transferred_cycles)
            );
        }
    }

    Ok(())
}

fn machine_sget(
    machine: DefaultMachine<DefaultCoreMachine<u64, WXorXMemory<FlatMemory<u64>>>>,
    context: Arc<Mutex<MachineContext>>,
    specify: usize,
) -> Result<DefaultMachine<DefaultCoreMachine<u64, WXorXMemory<FlatMemory<u64>>>>, Error> {
    let mut machine = machine;
    let mut decoder = build_decoder::<u64>(machine.isa(), machine.version());
    let mut specify = specify;
    let mut machine_vec = vec![];
    let mut spawn_data = None;
    while specify != 0 {
        decoder.reset_instructions_cache();
        while machine.running() {
            let opcode = {
                let pc = machine.pc().to_u64();
                let memory = machine.memory_mut();
                extract_opcode(decoder.decode(memory, pc)?)
            };
            if opcode == OP_ECALL && machine.registers()[A7] == 2043 {
                machine.step(&mut decoder)?;
                if machine.reset_signal() {
                    decoder.reset_instructions_cache()
                }
                specify -= 1;
                break;
            }
            if opcode == OP_ECALL && machine.registers()[A7] == 2101 {
                let cycles_current = machine.cycles();
                machine.set_cycles(machine.max_cycles() - 500);
                let cycles_diff = machine.cycles() - cycles_current;
                let _ = machine.step(&mut decoder);
                machine.set_cycles(cycles_current + 500);

                let machine_resumable = context.lock().unwrap().suspended_machines.pop().unwrap();
                if let ResumableMachine::Spawn(mut machine_child, spawn_data_child) = machine_resumable {
                    machine_child.machine.inner_mut().set_max_cycles(cycles_diff);
                    machine_vec.push(machine);
                    machine = machine_child.machine;
                    spawn_data = Some(spawn_data_child);
                }
                specify -= 1;
                break;
            }
            machine.step(&mut decoder)?;
        }
        if !machine.running() {
            machine = machine_vec.pop().unwrap();
            update_caller_machine(&mut machine, 0, 0, &spawn_data.clone().unwrap()).unwrap();
        }
    }
    return Ok(machine);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracedInstruction {
    pub opcode: String,
    pub instruction: Instruction,
    pub length: u32,
    pub mnemonics: String,
    pub op_a: u32,
    pub op_b: u32,
    pub op_c: u32,
    pub imm_b: bool,
    pub imm_c: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceItem {
    global_clk: u64,
    pc: u64,
    registers: Vec<u64>,
    instruction: TracedInstruction,
}

impl TraceItem {
    fn new(global_clk: u64, pc: u64, registers: Vec<u64>, instruction: TracedInstruction) -> Self {
        TraceItem {
            global_clk,
            pc,
            registers,
            instruction,
        }
    }
}

impl TracedInstruction {
    fn new(inst: TaggedInstruction) -> Self {
        let opcode = instruction_opcode_name(extract_opcode(inst.clone().into()))
            // remove _VERSION0 or _VERSION1
            .split("_")
            .next()
            .unwrap()
            .to_string();
        let instruction = inst.clone().into();
        let mnemonics = format!("{}", inst.clone());
        let length = instruction_length(instruction).into();
        match inst {
            TaggedInstruction::Itype(i) => TracedInstruction {
                opcode,
                instruction,
                length,
                mnemonics,
                op_a: i.rd() as u32,
                op_b: i.rs1() as u32,
                op_c: i.immediate_u(),
                imm_b: false,
                imm_c: true,
            },
            TaggedInstruction::Rtype(r) => TracedInstruction {
                opcode,
                instruction,
                length,
                mnemonics,
                op_a: r.rd() as u32,
                op_b: r.rs1() as u32,
                op_c: r.rs2() as u32,
                imm_b: false,
                imm_c: false,
            },
            TaggedInstruction::Stype(s) => TracedInstruction {
                opcode,
                instruction,
                length,
                mnemonics,
                op_a: s.rs2() as u32,
                op_b: s.rs1() as u32,
                op_c: s.immediate_u() as u32,
                imm_b: false,
                imm_c: false,
            },
            TaggedInstruction::Utype(u) => TracedInstruction {
                opcode,
                instruction,
                length,
                mnemonics,
                op_a: u.rd() as u32,
                op_b: u.immediate_u() as u32,
                op_c: 0,
                imm_b: true,
                imm_c: false,
            },
            TaggedInstruction::R4type(r4) => {
                panic!("Shouldn't have r4 type in trace dumping: {:?}", r4);
            }
            TaggedInstruction::R5type(r5) => {
                panic!("Shouldn't have r5 type in trace dumping: {:?}", r5);
            }
        }
    }
}
