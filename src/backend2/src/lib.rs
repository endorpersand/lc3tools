mod io;
mod sim;

use std::io::Write;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};

use lc3_ensemble::asm::{assemble_debug, ObjectFile};
use lc3_ensemble::ast::reg_consts::{R0, R1, R2, R3, R4, R5, R6, R7};
use lc3_ensemble::parse::parse_ast;
use lc3_ensemble::sim::debug::{Breakpoint, Comparator};
use lc3_ensemble::sim::io::BiChannelIO;
use lc3_ensemble::sim::mem::{MemAccessCtx, Word};
use lc3_ensemble::sim::{SimErr, Simulator};
use neon::prelude::*;
use io::{report_error, report_simple, InputBuffer, PrintBuffer};
use once_cell::sync::Lazy;
use sim::SimController;

/// Use [`input_buffer`].
static INPUT_BUFFER: Lazy<RwLock<InputBuffer>> = Lazy::new(|| RwLock::new(InputBuffer::new()));

/// Read guard to the input buffer.
/// 
/// This is all that's necessary for typical use because receives/sends can
/// be done with a shared reference.
/// 
/// The only operation that should use a write guard is the one to
/// replace the current `InputBuffer` when initializing the simulator's IO.
/// If the executing thread panics before this guard is released, 
/// the buffer is cleared.
fn input_buffer<'g>() -> RwLockReadGuard<'g, InputBuffer> {
    match INPUT_BUFFER.read() {
        Ok(g) => g,
        Err(e) => {
            // can't happen, the only poison that can occur is during write
            // and it can't panic there
            INPUT_BUFFER.clear_poison();
            e.into_inner()
        }
    }
}

/// Mutex guard to the print buffer.
/// 
/// If the executing thread panics before this guard is released, 
/// the buffer is cleared.
fn print_buffer<'g>() -> MutexGuard<'g, PrintBuffer> {
    static PRINT_BUFFER: Mutex<PrintBuffer> = Mutex::new(PrintBuffer::new());

    match PRINT_BUFFER.lock() {
        Ok(g) => g,
        Err(mut e) => {
            std::mem::take(&mut **e.get_mut());
            PRINT_BUFFER.clear_poison();
            e.into_inner()
        }
    }
}

/// Initializes the simulator's IO
fn init_io(sim: &mut Simulator) {
    use lc3_ensemble::sim::io::Stop;

    let mcr = Arc::clone(sim.mcr());

    // Reset input buffer.
    // By wiping the previous buffer, the reader thread of 
    // the previous simulator's IO should terminate (because the sender channel disconnected).
    // This means there shouldn't be a memory process risk!
    *INPUT_BUFFER.write().unwrap() = InputBuffer::new();

    let rx = input_buffer().rx();
    let io = BiChannelIO::new(
        move || rx.recv().map_err(|_| Stop),
        |byte| {
            let _ = print_buffer().write_all(&[byte]);
            Ok(())
        },
        mcr
    );
    sim.open_io(io);
}

static SIM_CONTENTS: Lazy<Mutex<SimPageContents>> = Lazy::new(|| {
    Mutex::new(SimPageContents {
        controller: SimController::new(false),
        obj_file: None
    })
});
struct SimPageContents {
    controller: SimController,
    obj_file: Option<ObjectFile>
}

//--------- CONFIG FUNCTIONS ---------//

fn init(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn() -> Result<()>
    // TODO: Determine whether ensemble requires an init.
    Ok(cx.undefined())
}
fn set_enable_liberal_asm(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (enable: bool) -> Result<()>
    // TODO: What does liberal ASM do?
    Ok(cx.undefined())
}
fn set_ignore_privilege(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(enable: bool) -> Result<()>
    // TODO: Implement ignore privilege
    Ok(cx.undefined())
}

//--------- CONSOLE FUNCTIONS ---------//

fn get_and_clear_output(mut cx: FunctionContext) -> JsResult<JsString> {
    // fn() -> Result<String>
    let string = print_buffer().take();
    Ok(cx.string(string))
}

fn clear_output(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn() -> Result<()>
    print_buffer().take();
    Ok(cx.undefined())
}

//--------- EDITOR/ASSEMBLER FUNCTIONS ---------//

fn convert_bin(mut _cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(fp: String) -> Result<()>

    // .bin files are files that have ASCII binary instead of assembly code.
    // Maybe will be implemented later? idk.
    unimplemented!("ConvertBin");
}

fn assemble(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fp: String) -> Result<()>
    let fp = cx.argument::<JsString>(0)?.value(&mut cx);
    let in_path = AsRef::<Path>::as_ref(&fp);
    let out_path = in_path.with_extension("obj");
    
    // should be unreachable cause frontend validates IO
    let src = std::fs::read_to_string(in_path).unwrap();

    let ast = parse_ast(&src)
        .map_err(|e| report_error(e, in_path, &src, &mut cx, &mut print_buffer()))?;
    let asm = assemble_debug(ast, &src)
        .map_err(|e| report_error(e, in_path, &src, &mut cx, &mut print_buffer()))?;
    
    std::fs::write(&out_path, asm.write_bytes())
        .map_err(|e| report_simple(in_path, e, &mut cx, &mut print_buffer()))?;

    writeln!(print_buffer(), "successfully assembled {} into {}", in_path.display(), out_path.display()).unwrap();
    Ok(cx.undefined())
}

//--------- SIMULATOR FUNCTIONS ---------//

fn get_curr_sym_table(mut cx: FunctionContext) -> JsResult<JsObject> {
    // fn () -> Result<Object>
    let obj = cx.empty_object();

    let contents = SIM_CONTENTS.lock().unwrap();
    let Some(obj_file) = contents.obj_file.as_ref() else { return Ok(obj) };
    let Some(_) = obj_file.symbol_table() else { return Ok(obj) };
    Ok(obj)
}
fn load_object_file(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fp: string) -> Result<()>
    let fp = cx.argument::<JsString>(0)?.value(&mut cx);
    let in_path = AsRef::<Path>::as_ref(&fp);
    
    // should be unreachable cause frontend validates IO
    let bytes = std::fs::read(in_path).unwrap();
    
    let Some(obj) = ObjectFile::read_bytes(&bytes) else {
        writeln!(print_buffer(), "error: malformed object file {fp}").unwrap();
        return Ok(cx.undefined());
    };

    let mut contents = match SIM_CONTENTS.lock() {
        Ok(c)  => c,
        Err(e) => e.into_inner(),
    };
    let sim = contents.controller.reset(false);
    init_io(sim);
    sim.load_obj_file(&obj);
    contents.obj_file.replace(obj);
    SIM_CONTENTS.clear_poison();

    Ok(cx.undefined())
}
fn restart_machine(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn () -> Result<()>
    
    // TODO: reset the simulator's PC + PSR

    Ok(cx.undefined())
}
fn reinitialize_machine(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn () -> Result<()>
    let mut contents = match SIM_CONTENTS.lock() {
        Ok(c)  => c,
        Err(e) => e.into_inner(),
    };
    let sim = contents.controller.reset(true);
    init_io(sim);
    SIM_CONTENTS.clear_poison();
    
    Ok(cx.undefined())
}
fn randomize_machine(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let mut contents = match SIM_CONTENTS.lock() {
        Ok(c)  => c,
        Err(e) => e.into_inner(),
    };
    let sim = contents.controller.reset(false);
    init_io(sim);
    SIM_CONTENTS.clear_poison();
    
    Ok(cx.undefined())
}

/// Helper that handles the result of the simulation and sends the error (if it exists)  back to the JS thread.
fn finish_execution(channel: Channel, cb: Root<JsFunction>, result: Result<(), SimErr>) {
    channel.send(move |mut cx| {
        let this = cx.undefined();
        let arg = cx.undefined().as_value(&mut cx);

        if let Err(e) = result {
            let pc = SIM_CONTENTS.lock().unwrap().controller.simulator().unwrap().prefetch_pc();
            writeln!(print_buffer(), "error: {e} (instruction x{pc:04X})").unwrap();
        }

        cb.into_inner(&mut cx)
            .call(&mut cx, this, vec![arg])?;

        Ok(())
    });
}
fn run(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let channel = cx.channel();
    let done_cb = cx.argument::<JsFunction>(0)?.root(&mut cx);

    SIM_CONTENTS.lock().unwrap().controller.execute(
        Simulator::run,
        |result| finish_execution(channel, done_cb, result)
    );

    Ok(cx.undefined())
}
fn run_until_halt(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let channel = cx.channel();
    let done_cb = cx.argument::<JsFunction>(0)?.root(&mut cx);

    SIM_CONTENTS.lock().unwrap().controller.execute(
        Simulator::run,
        |result| finish_execution(channel, done_cb, result)
    );

    Ok(cx.undefined())
}
fn step_in(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let channel = cx.channel();
    let done_cb = cx.argument::<JsFunction>(0)?.root(&mut cx);
    
    SIM_CONTENTS.lock().unwrap().controller.execute(
        Simulator::step_in,
        |result| finish_execution(channel, done_cb, result)
    );

    Ok(cx.undefined())
}
fn step_out(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let channel = cx.channel();
    let done_cb = cx.argument::<JsFunction>(0)?.root(&mut cx);
    
    SIM_CONTENTS.lock().unwrap().controller.execute(
        Simulator::step_out,
        |result| finish_execution(channel, done_cb, result)
    );

    Ok(cx.undefined())
}
fn step_over(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (fn(err) -> ()) -> Result<()>
    let channel = cx.channel();
    let done_cb = cx.argument::<JsFunction>(0)?.root(&mut cx);
    
    SIM_CONTENTS.lock().unwrap().controller.execute(
        Simulator::step_over,
        |result| finish_execution(channel, done_cb, result)
    );

    Ok(cx.undefined())
}
fn pause(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    SIM_CONTENTS.lock().unwrap().controller.pause();
    Ok(cx.undefined())
}

fn get_reg_value(mut cx: FunctionContext) -> JsResult<JsNumber> {
    // fn(reg: String) -> Result<u16>
    // reg here can be R0-7, PC, PSR, MCR
    let reg = cx.argument::<JsString>(0)?.value(&mut cx);

    let mut sim_contents = SIM_CONTENTS.lock().unwrap();
    let simulator = sim_contents.controller.simulator().unwrap();

    let value = match &*reg {
        "r0"  => simulator.reg_file[R0].get(),
        "r1"  => simulator.reg_file[R1].get(),
        "r2"  => simulator.reg_file[R2].get(),
        "r3"  => simulator.reg_file[R3].get(),
        "r4"  => simulator.reg_file[R4].get(),
        "r5"  => simulator.reg_file[R5].get(),
        "r6"  => simulator.reg_file[R6].get(),
        "r7"  => simulator.reg_file[R7].get(),
        "pc"  => simulator.pc,
        "psr" => simulator.psr().0,
        "mcr" => {
            let mcr = simulator.mcr();
            if mcr.load(Ordering::Relaxed) { 0x8000 } else { 0x0000 }
        }
        _ => panic!("undefined register")
    };
    std::mem::drop(sim_contents);
    
    Ok(cx.number(value))
}
fn set_reg_value(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(reg: String, value: u16) -> Result<()>
    // reg here can be R0-7, PC, PSR, MCR
    let reg = cx.argument::<JsString>(0)?.value(&mut cx);
    let value = cx.argument::<JsNumber>(1)?.value(&mut cx) as u16;

    let mut sim_contents = SIM_CONTENTS.lock().unwrap();
    let simulator = sim_contents.controller.simulator().unwrap();

    match &*reg {
        "r0"  => simulator.reg_file[R0].set(value),
        "r1"  => simulator.reg_file[R1].set(value),
        "r2"  => simulator.reg_file[R2].set(value),
        "r3"  => simulator.reg_file[R3].set(value),
        "r4"  => simulator.reg_file[R4].set(value),
        "r5"  => simulator.reg_file[R5].set(value),
        "r6"  => simulator.reg_file[R6].set(value),
        "r7"  => simulator.reg_file[R7].set(value),
        "pc"  => simulator.pc = value,
        "psr" => panic!("cannot set PSR"),
        "mcr" => {
            let mcr = simulator.mcr();
            mcr.store((value as i16) < 0, Ordering::Relaxed);
        }
        _ => panic!("undefined register")
    };
    std::mem::drop(sim_contents);
    
    Ok(cx.undefined())
}
fn get_mem_value(mut cx: FunctionContext) -> JsResult<JsNumber> {
    // fn (addr: u16) -> Result<u16>
    let addr = cx.argument::<JsNumber>(0)?.value(&mut cx) as u16;

    let mut sim_contents = SIM_CONTENTS.lock().unwrap();
    let simulator = sim_contents.controller.simulator().unwrap();

    let value = simulator.mem.get(addr, MemAccessCtx { privileged: true, strict: false, io: &simulator.io })
        .unwrap()
        .get();
    Ok(cx.number(value))
}
fn set_mem_value(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn (addr: u16, value: u16) -> Result<()>
    let addr  = cx.argument::<JsNumber>(0)?.value(&mut cx) as u16;
    let value = cx.argument::<JsNumber>(1)?.value(&mut cx) as u16;
    
    let mut sim_contents = SIM_CONTENTS.lock().unwrap();
    let simulator = sim_contents.controller.simulator().unwrap();

    simulator.mem.set(addr, Word::new_init(value), MemAccessCtx { privileged: true, strict: false, io: &simulator.io })
        .unwrap();
    
    Ok(cx.undefined())
}
fn get_mem_line(mut cx: FunctionContext) -> JsResult<JsString> {
    // fn(addr: u16) -> Result<String>
    let addr = cx.argument::<JsNumber>(0)?.value(&mut cx) as u16;
    let sim_contents = SIM_CONTENTS.lock().unwrap();
    
    'get_line: {
        let Some(obj) = &sim_contents.obj_file else { break 'get_line };
        let Some(sym) = obj.symbol_table() else { break 'get_line };
        let Some(src_info) = sym.source_info() else { break 'get_line };
    
        let Some(lno) = sym.find_source_line(addr) else { break 'get_line };
        let Some(lspan) = src_info.line_span(lno) else { break 'get_line };
        
        return Ok(cx.string(&src_info.source()[lspan]))
    }
    Ok(cx.string(""))
}
fn set_mem_line(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(addr: u16, value: String) -> Result<()>
    // TODO: implement
    Ok(cx.undefined())
}
fn clear_input(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn() -> ()
    let rx = input_buffer().rx();
    while rx.try_recv().is_ok() {}
    
    Ok(cx.undefined())
}

fn add_input(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(input: string) -> Result<()>
    // string is supposed to be char, though
    let input = cx.argument::<JsString>(0)?.value(&mut cx);
    
    let &[ch] = input.as_bytes() else {
        return cx.throw_error("more than one byte was sent at once");
    };
    input_buffer().send(ch);

    Ok(cx.undefined())
}

fn set_breakpoint(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(addr: u16) -> Result<()>
    let addr = cx.argument::<JsNumber>(0)?.value(&mut cx) as u16;
    SIM_CONTENTS.lock().unwrap()
        .controller
        .simulator()
        .unwrap()
        .breakpoints
        .push(Breakpoint::PC(Comparator::eq(addr)));
    Ok(cx.undefined())
}

fn remove_breakpoint(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    // fn(addr: u16) -> Result<()>
    let addr = cx.argument::<JsNumber>(0)?.value(&mut cx) as u16;
    SIM_CONTENTS.lock().unwrap()
        .controller
        .simulator()
        .unwrap()
        .breakpoints
        .retain(|bp| bp != &Breakpoint::PC(Comparator::eq(addr)));
    Ok(cx.undefined())
}

fn get_inst_exec_count(mut cx: FunctionContext) -> JsResult<JsNumber> {
    // fn() -> Result<usize>
    // I have no idea what this does
    Ok(cx.number(0))
}

fn did_hit_breakpoint(mut cx: FunctionContext) -> JsResult<JsBoolean> {
    // fn() -> Result<bool>
    // TODO: implement
    Ok(cx.boolean(false))
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("init", init)?;
    cx.export_function("convertBin", convert_bin)?;
    cx.export_function("assemble", assemble)?;
    cx.export_function("getCurrSymTable", get_curr_sym_table)?;
    cx.export_function("setEnableLiberalAsm", set_enable_liberal_asm)?;
    cx.export_function("loadObjectFile", load_object_file)?;
    cx.export_function("restartMachine", restart_machine)?;
    cx.export_function("reinitializeMachine", reinitialize_machine)?;
    cx.export_function("randomizeMachine", randomize_machine)?;
    cx.export_function("run", run)?;
    cx.export_function("runUntilHalt", run_until_halt)?;
    cx.export_function("stepIn", step_in)?;
    cx.export_function("stepOut", step_out)?;
    cx.export_function("stepOver", step_over)?;
    cx.export_function("pause", pause)?;
    cx.export_function("getRegValue", get_reg_value)?;
    cx.export_function("setRegValue", set_reg_value)?;
    cx.export_function("getMemValue", get_mem_value)?;
    cx.export_function("setMemValue", set_mem_value)?;
    cx.export_function("getMemLine", get_mem_line)?;
    cx.export_function("setMemLine", set_mem_line)?;
    cx.export_function("setIgnorePrivilege", set_ignore_privilege)?;
    cx.export_function("clearInput", clear_input)?;
    cx.export_function("addInput", add_input)?;
    cx.export_function("getAndClearOutput", get_and_clear_output)?;
    cx.export_function("clearOutput", clear_output)?;
    cx.export_function("setBreakpoint", set_breakpoint)?;
    cx.export_function("removeBreakpoint", remove_breakpoint)?;
    cx.export_function("getInstExecCount", get_inst_exec_count)?;
    cx.export_function("didHitBreakpoint", did_hit_breakpoint)?;
    Ok(())
}
