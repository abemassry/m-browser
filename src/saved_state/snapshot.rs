//! The generic engine: the wasm state of a core module's instance at a save
//! point, a moment when none of the module's functions is running.
//!
//! An instance's state is its memories, its mutable globals and its tables.
//! wasmtime's API reaches only what a module exports, and compilers export
//! little: C's stack pointer, for one, is a global nobody exports. So
//! `prepare` rewrites the module before it is loaded and adds an export for
//! every memory, table and mutable global the module defines, as Wizer
//! does: `__msave_memory_N`, `__msave_table_N`, `__msave_global_N`, N being
//! the item's index in the module.
//!
//! | State | Saved | Restored into a new instance |
//! |---|---|---|
//! | memories | size, and each 64 KiB chunk that is not all zeros | grown to the size, chunks copied in |
//! | mutable globals | values | set |
//! | tables | sizes, which must not have changed | checked |
//!
//! Immutable globals and the tables' contents are what instantiation makes
//! them, so a new instance has them already. A program that changes its
//! tables while it runs (`table.set`, `table.grow`, ...) cannot be saved:
//! the entries would be references to functions, which have no portable
//! form.
//!
//! A command (its `_start` runs to the end) has no save point. A command
//! transformed with Binaryen's Asyncify (`wasm-opt --asyncify`) can make
//! one: asked to, it copies its call stack into its own memory and returns
//! to the host, and when called again later it rebuilds the stack from
//! there. The helpers at the end of this file prepare the memory it needs.

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};
use wasmtime::{AsContextMut, Instance, Memory, Val};

use super::{read_u32, read_u64, SectionReader, StateWriter, TAG_GLOBALS, TAG_MEMORY, TAG_TABLES};

/// Prefix of the exports `prepare` adds.
pub const EXPORT_PREFIX: &str = "__msave_";
/// Memories are saved in chunks of this size; chunks of zeros are left out.
const CHUNK: usize = 64 * 1024;

pub fn memory_export(index: u32) -> String {
    format!("{EXPORT_PREFIX}memory_{index}")
}

pub fn global_export(index: u32) -> String {
    format!("{EXPORT_PREFIX}global_{index}")
}

pub fn table_export(index: u32) -> String {
    format!("{EXPORT_PREFIX}table_{index}")
}

/// What `prepare` found out about a module.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// Indices of the memories the module defines (imported ones are the
    /// host's business).
    pub memories: Vec<u32>,
    /// Indices of the mutable globals it defines.
    pub globals: Vec<u32>,
    /// Indices and declared sizes of the tables it defines.
    pub tables: Vec<(u32, u64)>,
    /// Why the module can never be saved, if it cannot.
    pub problems: Vec<String>,
    /// It was transformed by Asyncify (it exports `asyncify_*`).
    pub asyncify: bool,
}

pub struct Prepared {
    /// The module, with the added exports.
    pub wasm: Vec<u8>,
    pub layout: Layout,
}

/// Exports Asyncify adds to a module.
pub const ASYNCIFY_EXPORTS: [&str; 5] = [
    "asyncify_start_unwind",
    "asyncify_stop_unwind",
    "asyncify_start_rewind",
    "asyncify_stop_rewind",
    "asyncify_get_state",
];

/// Add the exports that make a module's whole state reachable, and find
/// out whether it can be saved at all.
pub fn prepare(wasm: &[u8]) -> Result<Prepared> {
    use wasmparser::{Encoding, Operator, Parser, Payload, TypeRef};

    let mut layout = Layout::default();
    let (mut imported_memories, mut imported_tables, mut imported_globals) = (0u32, 0u32, 0u32);
    let mut exports = Vec::new();
    let mut changes_tables = false;
    let mut parser = Parser::new(0);
    parser.set_features(wasmparser::WasmFeatures::all());
    for payload in parser.parse_all(wasm) {
        match payload.context("not a valid wasm module")? {
            Payload::Version { encoding: Encoding::Component, .. } => {
                bail!("this is a component, not a core module")
            }
            Payload::ImportSection(imports) => {
                for import in imports {
                    match import?.ty {
                        TypeRef::Memory(m) => {
                            if m.shared {
                                layout.problems.push(
                                    "it shares its memory with threads, whose running functions cannot be saved"
                                        .into(),
                                );
                            }
                            imported_memories += 1;
                        }
                        TypeRef::Table(_) => imported_tables += 1,
                        TypeRef::Global(_) => imported_globals += 1,
                        _ => {}
                    }
                }
            }
            Payload::MemorySection(memories) => {
                for (i, memory) in memories.into_iter().enumerate() {
                    if memory?.shared {
                        layout.problems.push("it has a shared memory (threads)".into());
                    }
                    layout.memories.push(imported_memories + i as u32);
                }
            }
            Payload::TableSection(tables) => {
                for (i, table) in tables.into_iter().enumerate() {
                    layout.tables.push((imported_tables + i as u32, table?.ty.initial));
                }
            }
            Payload::GlobalSection(globals) => {
                for (i, global) in globals.into_iter().enumerate() {
                    if global?.ty.mutable {
                        layout.globals.push(imported_globals + i as u32);
                    }
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    exports.push(export?.name.to_string());
                }
            }
            Payload::CodeSectionEntry(body) if !changes_tables => {
                let mut ops = body.get_operators_reader()?;
                while !ops.eof() {
                    if matches!(
                        ops.read()?,
                        Operator::TableSet { .. }
                            | Operator::TableGrow { .. }
                            | Operator::TableFill { .. }
                            | Operator::TableCopy { .. }
                            | Operator::TableInit { .. }
                            | Operator::ElemDrop { .. }
                    ) {
                        changes_tables = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(name) = exports.iter().find(|n| n.starts_with(EXPORT_PREFIX)) {
        bail!("the module already exports {name}: names starting with {EXPORT_PREFIX} are m-browser's");
    }
    if changes_tables {
        layout
            .problems
            .push("it changes its function tables while it runs (table.set, table.grow, ...)".into());
    }
    layout.asyncify = ASYNCIFY_EXPORTS.iter().all(|name| exports.iter().any(|e| e == name));

    let mut added = Vec::new();
    for &index in &layout.memories {
        added.push((memory_export(index), 0x02, index));
    }
    for &(index, _) in &layout.tables {
        added.push((table_export(index), 0x01, index));
    }
    for &index in &layout.globals {
        added.push((global_export(index), 0x03, index));
    }
    Ok(Prepared { wasm: add_exports(wasm, &added)?, layout })
}

/// Copy a module, adding exports (name, kind, index) to its export section.
/// All other sections, debug information included, are copied byte for
/// byte.
fn add_exports(wasm: &[u8], added: &[(String, u8, u32)]) -> Result<Vec<u8>> {
    let mut entries = Vec::new();
    for (name, kind, index) in added {
        write_leb(&mut entries, name.len() as u32);
        entries.extend_from_slice(name.as_bytes());
        entries.push(*kind);
        write_leb(&mut entries, *index);
    }
    let export_section = |old_count: u32, old_entries: &[u8]| {
        let mut body = Vec::new();
        write_leb(&mut body, old_count + added.len() as u32);
        body.extend_from_slice(old_entries);
        body.extend_from_slice(&entries);
        let mut section = vec![7u8];
        write_leb(&mut section, body.len() as u32);
        section.extend_from_slice(&body);
        section
    };

    let mut out = Vec::with_capacity(wasm.len() + entries.len() + 16);
    out.extend_from_slice(wasm.get(..8).context("not a wasm module")?);
    let mut pos = 8;
    let mut written = false;
    while pos < wasm.len() {
        let start = pos;
        let id = wasm[pos];
        pos += 1;
        let size = read_leb(wasm, &mut pos)? as usize;
        let end = pos.checked_add(size).filter(|&end| end <= wasm.len()).context("truncated module")?;
        if id == 7 {
            let mut entries_at = pos;
            let count = read_leb(wasm, &mut entries_at)?;
            out.extend_from_slice(&export_section(count, &wasm[entries_at..end]));
            written = true;
        } else {
            // A module without exports gets the section where the order of
            // sections wants it: before start, element, data count, code
            // and data.
            if !written && matches!(id, 8 | 9 | 10 | 11 | 12) {
                out.extend_from_slice(&export_section(0, &[]));
                written = true;
            }
            out.extend_from_slice(&wasm[start..end]);
        }
        pos = end;
    }
    if !written {
        out.extend_from_slice(&export_section(0, &[]));
    }
    Ok(out)
}

fn write_leb(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_leb(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *bytes.get(*pos).context("truncated module")?;
        *pos += 1;
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("malformed module (LEB128 number too long)")
}

/// A global's value in a saved state.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
enum SavedVal {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128(u128),
    NullFunc,
    NullExtern,
    NullAny,
}

/// Save an instance's wasm state into `w`: sections GLOB and TABL, and a
/// MEMO for each memory. Only call it at a save point.
pub fn save(mut store: impl AsContextMut, instance: &Instance, layout: &Layout, w: &mut StateWriter) -> Result<()> {
    if let Some(problem) = layout.problems.first() {
        bail!("this program cannot be saved: {problem}");
    }
    let mut globals = Vec::with_capacity(layout.globals.len());
    for &index in &layout.globals {
        let global = instance
            .get_global(&mut store, &global_export(index))
            .with_context(|| format!("global {index} is not exported: was the module prepared?"))?;
        let value = match global.get(&mut store) {
            Val::I32(v) => SavedVal::I32(v),
            Val::I64(v) => SavedVal::I64(v),
            Val::F32(v) => SavedVal::F32(v),
            Val::F64(v) => SavedVal::F64(v),
            Val::V128(v) => SavedVal::V128(v.as_u128()),
            Val::FuncRef(None) => SavedVal::NullFunc,
            Val::ExternRef(None) => SavedVal::NullExtern,
            Val::AnyRef(None) => SavedVal::NullAny,
            _ => bail!("global {index} holds a reference to a function or a host object, which cannot be saved"),
        };
        globals.push((index, value));
    }
    w.section(TAG_GLOBALS, &postcard::to_allocvec(&globals)?)?;

    let mut tables = Vec::with_capacity(layout.tables.len());
    for &(index, declared) in &layout.tables {
        let table = instance
            .get_table(&mut store, &table_export(index))
            .with_context(|| format!("table {index} is not exported: was the module prepared?"))?;
        let size = table.size(&store);
        if size != declared {
            bail!("table {index} grew from {declared} to {size} entries while the program ran; they cannot be saved");
        }
        tables.push((index, size));
    }
    w.section(TAG_TABLES, &postcard::to_allocvec(&tables)?)?;

    for &index in &layout.memories {
        let memory = instance
            .get_memory(&mut store, &memory_export(index))
            .with_context(|| format!("memory {index} is not exported: was the module prepared?"))?;
        let page_size = memory.page_size(&store);
        save_memory(index, memory.data(&store), page_size, w)?;
    }
    Ok(())
}

/// A MEMO section: index u32, size u64, page size u64, then for each chunk
/// that is not all zeros: chunk number u32, length u32, compressed length
/// u32, deflate data; then u32::MAX.
fn save_memory(index: u32, data: &[u8], page_size: u64, w: &mut StateWriter) -> Result<()> {
    let mut section = w.begin(TAG_MEMORY)?;
    section.write_all(&index.to_le_bytes())?;
    section.write_all(&(data.len() as u64).to_le_bytes())?;
    section.write_all(&page_size.to_le_bytes())?;
    let mut compressed = Vec::with_capacity(CHUNK);
    for (number, chunk) in data.chunks(CHUNK).enumerate() {
        if is_zero(chunk) {
            continue;
        }
        compressed.clear();
        let mut encoder = DeflateEncoder::new(&mut compressed, flate2::Compression::fast());
        encoder.write_all(chunk)?;
        encoder.finish()?;
        section.write_all(&(number as u32).to_le_bytes())?;
        section.write_all(&(chunk.len() as u32).to_le_bytes())?;
        section.write_all(&(compressed.len() as u32).to_le_bytes())?;
        section.write_all(&compressed)?;
    }
    section.write_all(&u32::MAX.to_le_bytes())?;
    section.end()
}

fn is_zero(chunk: &[u8]) -> bool {
    // SAFETY: any bit pattern is a valid u64.
    let (head, words, tail) = unsafe { chunk.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Puts saved wasm state into a new instance of the same prepared module,
/// one section at a time, as they come out of the file.
pub struct Restore<'a> {
    layout: &'a Layout,
    globals: bool,
    tables: bool,
    memories: Vec<u32>,
}

impl<'a> Restore<'a> {
    pub fn new(layout: &'a Layout) -> Self {
        Restore { layout, globals: false, tables: false, memories: Vec::new() }
    }

    /// Restore from `section` if it is one of this engine's; `false` if not.
    pub fn section(
        &mut self,
        mut store: impl AsContextMut,
        instance: &Instance,
        section: &mut SectionReader<'_>,
    ) -> Result<bool> {
        match section.tag {
            TAG_GLOBALS => {
                let globals: Vec<(u32, SavedVal)> = postcard::from_bytes(&section.read_rest()?)?;
                let indices: Vec<u32> = globals.iter().map(|(index, _)| *index).collect();
                if indices != self.layout.globals {
                    bail!("the saved globals do not match the module's");
                }
                for (index, value) in globals {
                    let value = match value {
                        SavedVal::I32(v) => Val::I32(v),
                        SavedVal::I64(v) => Val::I64(v),
                        SavedVal::F32(v) => Val::F32(v),
                        SavedVal::F64(v) => Val::F64(v),
                        SavedVal::V128(v) => Val::V128(v.into()),
                        SavedVal::NullFunc => Val::FuncRef(None),
                        SavedVal::NullExtern => Val::ExternRef(None),
                        SavedVal::NullAny => Val::AnyRef(None),
                    };
                    let global = instance
                        .get_global(&mut store, &global_export(index))
                        .with_context(|| format!("the module has no global {index}"))?;
                    global.set(&mut store, value).with_context(|| format!("cannot restore global {index}"))?;
                }
                self.globals = true;
            }
            TAG_TABLES => {
                let tables: Vec<(u32, u64)> = postcard::from_bytes(&section.read_rest()?)?;
                if tables != self.layout.tables {
                    bail!("the saved tables do not match the module's");
                }
                self.tables = true;
            }
            TAG_MEMORY => {
                let index = restore_memory(&mut store, instance, section)?;
                self.memories.push(index);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Check that the file held the whole wasm state.
    pub fn finish(self) -> Result<()> {
        if !self.globals || !self.tables {
            bail!("the saved state is incomplete: its globals or tables are missing");
        }
        for index in &self.layout.memories {
            if !self.memories.contains(index) {
                bail!("the saved state is incomplete: memory {index} is missing");
            }
        }
        Ok(())
    }
}

fn restore_memory(mut store: impl AsContextMut, instance: &Instance, r: &mut impl Read) -> Result<u32> {
    let index = read_u32(r)?;
    let length = usize::try_from(read_u64(r)?)?;
    let page_size = read_u64(r)?;
    let memory = instance
        .get_memory(&mut store, &memory_export(index))
        .with_context(|| format!("the module has no memory {index}"))?;
    if memory.page_size(&store) != page_size {
        bail!("memory {index} has another page size than when it was saved");
    }
    let fresh = memory.data_size(&store);
    if length < fresh {
        bail!("memory {index} had {length} bytes when saved, fewer than a new instance has ({fresh})");
    }
    if length > fresh {
        memory
            .grow(&mut store, (length - fresh) as u64 / page_size)
            .with_context(|| format!("cannot grow memory {index} to its saved size, {length} bytes"))?;
    }
    let data = memory.data_mut(&mut store);
    // A new instance's memory holds the module's data segments; the saved
    // image replaces them, and it leaves out chunks that were all zeros.
    data[..fresh].fill(0);
    let mut compressed = Vec::with_capacity(CHUNK);
    loop {
        let number = read_u32(r)?;
        if number == u32::MAX {
            return Ok(index);
        }
        let raw = read_u32(r)? as usize;
        let size = read_u32(r)? as usize;
        let start = number as usize * CHUNK;
        if raw > CHUNK || size > 2 * CHUNK || start.checked_add(raw).is_none_or(|end| end > data.len()) {
            bail!("memory {index}: chunk {number} is damaged");
        }
        compressed.resize(size, 0);
        r.read_exact(&mut compressed)?;
        DeflateDecoder::new(&compressed[..])
            .read_exact(&mut data[start..start + raw])
            .with_context(|| format!("memory {index}: chunk {number} is damaged"))?;
    }
}

/// Bytes kept for an unwound call stack.
pub const ASYNCIFY_BUFFER_SIZE: u32 = 1 << 20;

/// Where an Asyncify program unwound its stack, saved with it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AsyncifySaved {
    /// The buffer holding the unwound stack, in memory 0.
    pub buffer: u32,
    /// The import the program was calling; on resume it continues there.
    pub import: String,
}

/// Room for an unwound stack: new pages at the end of `memory`. The
/// program's own allocator never hands them out, since it only uses pages
/// it grew itself.
pub fn asyncify_buffer(mut store: impl AsContextMut, memory: Memory) -> Result<u32> {
    let page = memory.page_size(&store);
    let pages = u64::from(ASYNCIFY_BUFFER_SIZE).div_ceil(page);
    let old = memory.grow(&mut store, pages).context("no room in memory for the unwound stack")?;
    u32::try_from(old * page).context("the unwound stack would lie beyond 4 GiB")
}

/// Write the two words Asyncify expects at the start of its buffer: where
/// the stack data begins and where the buffer ends.
pub fn asyncify_reset(mut store: impl AsContextMut, memory: Memory, buffer: u32) -> Result<()> {
    let header = [(buffer + 8).to_le_bytes(), (buffer + ASYNCIFY_BUFFER_SIZE).to_le_bytes()].concat();
    memory
        .write(&mut store, buffer as usize, &header)
        .context("the unwound stack's buffer is outside memory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saved_state::tests::{header, TestDir};
    use crate::saved_state::StateReader;
    use wasmtime::{Caller, Engine, Linker, Module, Store};

    fn prepare_wat(wat: &str) -> Prepared {
        let prepared = prepare(&wat::parse_str(wat).unwrap()).unwrap();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&prepared.wasm)
            .expect("the prepared module is valid");
        prepared
    }

    #[test]
    fn hidden_state_is_exported() {
        let prepared = prepare_wat(
            r#"(module
                (import "env" "g" (global i32))
                (memory 1)
                (table 2 funcref)
                (global (mut i32) (i32.const 1))
                (global i64 (i64.const 2))
                (global (mut f64) (f64.const 3))
                (func $f)
                (elem (i32.const 0) $f))"#,
        );
        let layout = &prepared.layout;
        assert_eq!(layout.memories, [0]);
        assert_eq!(layout.tables, [(0, 2)]);
        assert_eq!(layout.globals, [1, 3]);
        assert!(layout.problems.is_empty() && !layout.asyncify);

        let engine = Engine::default();
        let module = Module::new(&engine, &prepared.wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        let imported = wasmtime::Global::new(
            &mut store,
            wasmtime::GlobalType::new(wasmtime::ValType::I32, wasmtime::Mutability::Const),
            Val::I32(0),
        )
        .unwrap();
        linker.define(&store, "env", "g", imported).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let global = instance.get_global(&mut store, "__msave_global_3").unwrap();
        assert_eq!(global.get(&mut store).unwrap_f64(), 3.0);
        assert!(instance.get_memory(&mut store, "__msave_memory_0").is_some());
        assert!(instance.get_table(&mut store, "__msave_table_0").is_some());
    }

    #[test]
    fn existing_exports_and_sections_are_kept() {
        let prepared = prepare_wat(
            r#"(module
                (memory (export "memory") 1)
                (global $g (mut i32) (i32.const 5))
                (func $init (global.set $g (i32.const 6)))
                (func (export "get") (result i32) (global.get $g))
                (start $init)
                (data (i32.const 0) "x"))"#,
        );
        let engine = Engine::default();
        let module = Module::new(&engine, &prepared.wasm).unwrap();
        let names: Vec<&str> = module.exports().map(|e| e.name()).collect();
        assert_eq!(names, ["memory", "get", "__msave_memory_0", "__msave_global_0"]);
    }

    #[test]
    fn modules_that_cannot_be_saved_are_recognized() {
        let threads = prepare_wat(r#"(module (import "env" "memory" (memory 1 1 shared)))"#);
        assert!(threads.layout.problems[0].contains("threads"), "{:?}", threads.layout.problems);

        let tables = prepare_wat(
            r#"(module (table 1 funcref)
                (func (table.set 0 (i32.const 0) (ref.null func))))"#,
        );
        assert!(tables.layout.problems[0].contains("tables"), "{:?}", tables.layout.problems);

        let component = wat::parse_str("(component)").unwrap();
        assert!(prepare(&component).err().unwrap().to_string().contains("component"));

        let taken = wat::parse_str(r#"(module (func (export "__msave_x")))"#).unwrap();
        assert!(prepare(&taken).is_err());

        let asyncified = prepare_wat(include_str!("testdata/count.asyncify.wat"));
        assert!(asyncified.layout.asyncify && asyncified.layout.problems.is_empty());
    }

    /// Steps a counter kept in memory and in a hidden global, changes a data
    /// segment, zeroes another one and grows its memory.
    const REACTOR: &str = r#"(module
        (memory 2)
        (global $hidden (mut i32) (i32.const 100))
        (data (i32.const 1000) "first segment")
        (data (i32.const 65536) "second segment")
        (func (export "step") (result i32)
          (i32.store (i32.const 16) (i32.add (i32.load (i32.const 16)) (i32.const 1)))
          (global.set $hidden (i32.add (global.get $hidden) (i32.load (i32.const 16))))
          (i32.store8 (i32.const 1000) (i32.add (i32.load (i32.const 16)) (i32.const 64)))
          (if (i32.eq (i32.load (i32.const 16)) (i32.const 2))
            (then
              (memory.fill (i32.const 65536) (i32.const 0) (i32.const 14))
              (drop (memory.grow (i32.const 1)))
              (i32.store (i32.const 140000) (i32.const 55))))
          (i32.add (i32.mul (i32.load (i32.const 16)) (i32.const 100000)) (global.get $hidden)))
        (func (export "peek") (param i32) (result i32) (i32.load8_u (local.get 0)))
        (func (export "pages") (result i32) (memory.size)))"#;

    fn instantiate(prepared: &Prepared) -> (Store<()>, Instance) {
        let engine = Engine::default();
        let module = Module::new(&engine, &prepared.wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &module).unwrap();
        (store, instance)
    }

    fn restore<T>(path: &std::path::Path, store: &mut Store<T>, instance: &Instance, layout: &Layout) {
        let mut reader = StateReader::open(path).unwrap();
        let mut restore = Restore::new(layout);
        while let Some(mut section) = reader.next().unwrap() {
            assert!(restore.section(&mut *store, instance, &mut section).unwrap());
            section.end().unwrap();
        }
        restore.finish().unwrap();
    }

    #[test]
    fn a_reactor_continues_in_a_new_instance() {
        let dir = TestDir::new("snapshot-reactor");
        let path = dir.0.join("reactor.wasmsavestate");
        let prepared = prepare_wat(REACTOR);
        {
            let (mut store, instance) = instantiate(&prepared);
            let step = instance.get_typed_func::<(), i32>(&mut store, "step").unwrap();
            let results: Vec<i32> = (0..3).map(|_| step.call(&mut store, ()).unwrap()).collect();
            assert_eq!(results, [100_101, 200_103, 300_106]);
            let mut w = StateWriter::create(&path, &header()).unwrap();
            save(&mut store, &instance, &prepared.layout, &mut w).unwrap();
            w.finish().unwrap();
        }
        // A new engine and store, as in another process.
        let (mut store, instance) = instantiate(&prepared);
        restore(&path, &mut store, &instance, &prepared.layout);
        let call = |store: &mut Store<()>, name: &str, arg: Option<i32>| -> i32 {
            match arg {
                Some(a) => instance.get_typed_func::<i32, i32>(&mut *store, name).unwrap().call(store, a).unwrap(),
                None => instance.get_typed_func::<(), i32>(&mut *store, name).unwrap().call(store, ()).unwrap(),
            }
        };
        assert_eq!(call(&mut store, "step", None), 400_110);
        assert_eq!(call(&mut store, "pages", None), 3);
        assert_eq!(call(&mut store, "peek", Some(1000)), 64 + 4);
        assert_eq!(call(&mut store, "peek", Some(1001)), b'i' as i32);
        assert_eq!(call(&mut store, "peek", Some(65536)), 0, "a segment zeroed before the save stays zero");
        assert_eq!(call(&mut store, "peek", Some(140_000)), 55);
    }

    #[test]
    fn a_damaged_or_foreign_state_is_refused() {
        let dir = TestDir::new("snapshot-foreign");
        let path = dir.0.join("reactor.wasmsavestate");
        let prepared = prepare_wat(REACTOR);
        let (mut store, instance) = instantiate(&prepared);
        let mut w = StateWriter::create(&path, &header()).unwrap();
        save(&mut store, &instance, &prepared.layout, &mut w).unwrap();
        w.finish().unwrap();

        let other = prepare_wat(r#"(module (memory 1) (global i32 (i32.const 0)) (global (mut i64) (i64.const 0)))"#);
        let (mut store, instance) = instantiate(&other);
        let mut reader = StateReader::open(&path).unwrap();
        let mut restore = Restore::new(&other.layout);
        let mut section = reader.next().unwrap().unwrap();
        let e = restore.section(&mut store, &instance, &mut section).unwrap_err();
        assert!(e.to_string().contains("globals do not match"), "{e}");
    }

    #[derive(Default)]
    struct Ticks {
        save_at: Option<i32>,
        buffer: Option<u32>,
        unwound: bool,
        rewinding: bool,
    }

    fn call_export(caller: &mut Caller<'_, Ticks>, name: &str, args: &[Val]) {
        let f = caller.get_export(name).and_then(|e| e.into_func()).unwrap();
        f.call(&mut *caller, args, &mut []).unwrap();
    }

    fn ticking(prepared: &Prepared, ticks: Ticks) -> (Store<Ticks>, Instance) {
        let engine = Engine::default();
        let module = Module::new(&engine, &prepared.wasm).unwrap();
        let mut linker = Linker::new(&engine);
        linker
            .func_wrap("env", "tick", |mut caller: Caller<'_, Ticks>, n: i32| -> i32 {
                if caller.data().rewinding {
                    caller.data_mut().rewinding = false;
                    call_export(&mut caller, "asyncify_stop_rewind", &[]);
                } else if caller.data().save_at == Some(n) {
                    let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let buffer = asyncify_buffer(&mut caller, memory).unwrap();
                    asyncify_reset(&mut caller, memory, buffer).unwrap();
                    caller.data_mut().buffer = Some(buffer);
                    caller.data_mut().unwound = true;
                    call_export(&mut caller, "asyncify_start_unwind", &[Val::I32(buffer as i32)]);
                    return 0;
                }
                n * 100
            })
            .unwrap();
        let mut store = Store::new(&engine, ticks);
        let instance = linker.instantiate(&mut store, &module).unwrap();
        (store, instance)
    }

    #[test]
    fn an_asyncified_command_continues_in_a_new_instance() {
        let dir = TestDir::new("snapshot-asyncify");
        let path = dir.0.join("count.wasmsavestate");
        let prepared = prepare_wat(include_str!("testdata/count.asyncify.wat"));
        let buffer = {
            let (mut store, instance) = ticking(&prepared, Ticks { save_at: Some(4), ..Default::default() });
            let start = instance.get_typed_func::<(), ()>(&mut store, "_start").unwrap();
            start.call(&mut store, ()).unwrap();
            assert!(store.data().unwound, "_start returned because it unwound");
            let stop = instance.get_typed_func::<(), ()>(&mut store, "asyncify_stop_unwind").unwrap();
            stop.call(&mut store, ()).unwrap();
            let total = instance.get_typed_func::<(), i32>(&mut store, "total").unwrap();
            assert_eq!(total.call(&mut store, ()).unwrap(), 618, "0..=3 counted before the save");
            let mut w = StateWriter::create(&path, &header()).unwrap();
            save(&mut store, &instance, &prepared.layout, &mut w).unwrap();
            w.finish().unwrap();
            store.data().buffer.unwrap()
        };
        let (mut store, instance) = ticking(&prepared, Ticks { rewinding: true, ..Default::default() });
        restore(&path, &mut store, &instance, &prepared.layout);
        let rewind = instance.get_typed_func::<i32, ()>(&mut store, "asyncify_start_rewind").unwrap();
        rewind.call(&mut store, buffer as i32).unwrap();
        let start = instance.get_typed_func::<(), ()>(&mut store, "_start").unwrap();
        start.call(&mut store, ()).unwrap();
        let total = instance.get_typed_func::<(), i32>(&mut store, "total").unwrap();
        assert_eq!(total.call(&mut store, ()).unwrap(), 4635, "the same as a run without a save");
    }
}
