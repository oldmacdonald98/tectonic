// Copyright 2020-2021 the Tectonic Project
// Licensed under the MIT License.

#![deny(missing_docs)]
#![allow(clippy::assertions_on_constants)]

//! The [bibtex] program as a reusable crate.
//!
//! [bibtex]: http://www.bibtex.org/
//!
//! This crate provides the basic BibTeX implementation used by [Tectonic].
//! However, in order to obtain the full Tectonic user experience, it must be
//! combined with a variety of other utilities: the main XeTeX engine, code to
//! fetch support files, and so on. Rather than using this crate directly you
//! should probably use the main [`tectonic`] crate, which combines all of these
//! pieces into a (semi) coherent whole.
//!
//! [Tectonic]: https://tectonic-typesetting.github.io/
//! [`tectonic`]: https://docs.rs/tectonic/

use auxi::{with_aux_mut, AuxData};
use bibs::{with_bibs_mut, BibData};
use buffer::{with_buffers_mut, GlobalBuffer};
use cite::{with_cites_mut, CiteInfo};
use entries::{with_entries_mut, EntryData};
use external::*;
use global::{with_globals_mut, GlobalData};
use hash::{with_hash_mut, HashData};
use history::History;
use log::{init_log_file, print_confusion, sam_wrong_file_name_print, write_logs};
use other::OtherData;
use peekable::PeekableInput;
use pool::{with_pool_mut, StringPool};
use std::{
    ffi::{CStr, CString},
    ptr::{self, NonNull},
};
use tectonic_bridge_core::{CoreBridgeLauncher, CoreBridgeState, FileFormat};
use tectonic_errors::prelude::*;
use tectonic_io_base::{InputHandle, OutputHandle};
use xbuf::SafelyZero;

use crate::{
    auxi::{get_aux_command_and_process, last_check_for_aux_errors, pop_the_aux_stack},
    bst::get_bst_command_and_process,
    buffer::BufTy,
    exec::ExecCtx,
    history::{err_count, get_history},
    log::{bib_close_log, init_standard_output, log_pr_aux_name, print_aux_name, write_log_file},
    other::with_other_mut,
    peekable::{input_ln, peekable_close},
    pool::pre_def_certain_strings,
    scan::eat_bst_white_space,
};

pub(crate) mod auxi;
pub(crate) mod bibs;
pub(crate) mod bst;
pub(crate) mod buffer;
pub(crate) mod char_info;
pub(crate) mod cite;
pub(crate) mod entries;
pub(crate) mod exec;
pub(crate) mod global;
pub(crate) mod hash;
pub(crate) mod history;
pub(crate) mod log;
pub(crate) mod other;
pub(crate) mod peekable;
pub(crate) mod pool;
pub(crate) mod scan;
pub(crate) mod xbuf;

#[doc(hidden)]
#[derive(Debug)]
pub(crate) enum BibtexError {
    Fatal,
    Recover,
    NoBst,
}

/// A possible outcome from a BibTeX engine invocation.
///
/// The classic TeX implementation provides a fourth outcome: “fatal error”. In
/// Tectonic, this outcome is represented as an `Err` result rather than a
/// [`BibtexOutcome`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BibtexOutcome {
    /// Nothing bad happened.
    Spotless = 0,

    /// Warnings were issued.
    Warnings = 1,

    /// Errors occurred. Note that, in TeX usage, “errors” are not necessarily
    /// *fatal* errors: the engine will proceed and work around errors as best
    /// it can.
    Errors = 2,
}

/// A struct for invoking the BibTeX engine.
///
/// This struct has a fairly straightforward “builder” interface: you create it,
/// apply any settings that you wish, and eventually run the
/// [`process()`](Self::process) method.
///
/// Due to constraints of the gnarly C/C++ code underlying the engine
/// implementation, only one engine may run at once in one process. The engine
/// execution framework uses a global mutex to ensure that this is the case.
/// This restriction applies not only to the [`BibtexEngine`] type but to *all*
/// Tectonic engines. I.e., you can't run this engine and the XeTeX engine at
/// the same time.
#[derive(Debug, Default)]
pub struct BibtexEngine {
    config: BibtexConfig,
}

impl BibtexEngine {
    /// Set the BibTeX `min_crossrefs` parameter.
    ///
    /// The default value is 2.
    ///
    /// This needs verifying, but I believe that this setting affects how many
    /// times an item needs to be referenced in directly-referenced BibTeX
    /// entries before it gets its own standalone entry.
    pub fn min_crossrefs(&mut self, value: u32) -> &mut Self {
        self.config.min_crossrefs = value;
        self
    }

    /// Run BibTeX.
    ///
    /// The *launcher* parameter gives overarching environmental context in
    /// which the engine will be run.
    ///
    /// The *aux* parameter gives the name of the "aux" file, created by the TeX
    /// engine, that BibTeX will process.
    pub fn process(
        &mut self,
        launcher: &mut CoreBridgeLauncher<'_>,
        aux: &str,
    ) -> Result<BibtexOutcome> {
        let caux = CString::new(aux)?;

        launcher.with_global_lock(|state| {
            let mut ctx = Bibtex::new(state, self.config.clone());
            let hist = bibtex_main(&mut ctx, &caux);

            match hist {
                History::Spotless => Ok(BibtexOutcome::Spotless),
                History::WarningIssued => Ok(BibtexOutcome::Warnings),
                History::ErrorIssued => Ok(BibtexOutcome::Errors),
                History::FatalError => Err(anyhow!("unspecified fatal bibtex error")),
            }
        })
    }
}

// These used to be 'bad' checks at the start of a program, now we can ensure them at comptime
const _: () = assert!(hash::HASH_PRIME >= 128);
const _: () = assert!(pool::MAX_PRINT_LINE > pool::MIN_PRINT_LINE);
const _: () = assert!(pool::MIN_PRINT_LINE >= 3);
const _: () = assert!(pool::MAX_PRINT_LINE < buffer::BUF_SIZE + 1);
const _: () = assert!(hash::HASH_PRIME <= hash::HASH_SIZE);
const _: () = assert!(pool::MAX_STRINGS <= hash::HASH_SIZE);
const _: () = assert!(cite::MAX_CITES <= pool::MAX_STRINGS);

pub(crate) struct GlobalItems<'a> {
    buffers: &'a mut GlobalBuffer,
    pool: &'a mut StringPool,
    hash: &'a mut HashData,
    entries: &'a mut EntryData,
    globals: &'a mut GlobalData,
    bibs: &'a mut BibData,
    aux: &'a mut AuxData,
    cites: &'a mut CiteInfo,
    other: &'a mut OtherData,
}

impl GlobalItems<'_> {
    fn with<T>(f: impl FnOnce(&mut GlobalItems<'_>) -> T) -> T {
        with_buffers_mut(|buffers| {
            with_pool_mut(|pool| {
                with_hash_mut(|hash| {
                    with_entries_mut(|entries| {
                        with_globals_mut(|globals| {
                            with_bibs_mut(|bibs| {
                                with_aux_mut(|aux| {
                                    with_cites_mut(|cites| {
                                        with_other_mut(|other| {
                                            let mut globals = GlobalItems {
                                                buffers,
                                                pool,
                                                hash,
                                                entries,
                                                globals,
                                                bibs,
                                                aux,
                                                cites,
                                                other,
                                            };

                                            f(&mut globals)
                                        })
                                    })
                                })
                            })
                        })
                    })
                })
            })
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BibtexConfig {
    pub min_crossrefs: u32,
    pub verbose: bool,
}

impl Default for BibtexConfig {
    fn default() -> Self {
        BibtexConfig {
            min_crossrefs: 2,
            verbose: false,
        }
    }
}

pub(crate) struct Bibtex<'a, 'cbs> {
    pub engine: &'a mut CoreBridgeState<'cbs>,
    pub config: BibtexConfig,
    pub bst_file: Option<NonNull<PeekableInput>>,
    pub bst_str: StrNumber,
    pub bst_line_num: usize,

    pub bbl_file: *mut OutputHandle,
    pub bbl_line_num: usize,

    pub num_bib_files: usize,
    pub num_preamble_strings: usize,
    pub impl_fn_num: usize,
    pub cite_xptr: usize,

    pub bib_seen: bool,
    pub bst_seen: bool,
    pub citation_seen: bool,
    pub entry_seen: bool,
    pub read_seen: bool,
    pub read_performed: bool,
    pub reading_completed: bool,
    pub all_entries: bool,

    pub b_default: HashPointer,
    pub s_null: HashPointer,
    pub s_default: HashPointer,
    pub s_aux_extension: HashPointer,
}

impl<'a, 'cbs> Bibtex<'a, 'cbs> {
    pub(crate) fn new(
        engine: &'a mut CoreBridgeState<'cbs>,
        config: BibtexConfig,
    ) -> Bibtex<'a, 'cbs> {
        Bibtex {
            engine,
            config,
            bst_file: None,
            bst_str: 0,
            bst_line_num: 0,
            bbl_file: ptr::null_mut(),
            bbl_line_num: 0,
            num_bib_files: 0,
            num_preamble_strings: 0,
            impl_fn_num: 0,
            cite_xptr: 0,
            bib_seen: false,
            bst_seen: false,
            citation_seen: false,
            entry_seen: false,
            read_seen: false,
            read_performed: false,
            reading_completed: false,
            all_entries: false,
            b_default: 0,
            s_null: 0,
            s_default: 0,
            s_aux_extension: 0,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct LookupRes {
    /// The location of the string - where it exists, was inserted, of if insert is false,
    /// where it *would* have been inserted
    loc: usize,
    /// Whether the string existed in the hash table already
    exists: bool,
}

#[derive(Debug)]
pub(crate) struct FindCiteLocs {
    cite_loc: CiteNumber,
    lc_cite_loc: CiteNumber,

    cite_found: bool,
    lc_found: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StrIlk {
    Text,
    Integer,
    AuxCommand,
    AuxFile,
    BstCommand,
    BstFile,
    BibFile,
    FileExt,
    Cite,
    LcCite,
    BstFn,
    BibCommand,
    Macro,
    ControlSeq,
}

// SAFETY: StrIlk is valid at zero as StrIlk::Text
unsafe impl SafelyZero for StrIlk {}

type StrNumber = usize;
type CiteNumber = usize;
type ASCIICode = u8;
type BufPointer = usize;
type PoolPointer = usize;
type HashPointer = usize;
type AuxNumber = usize;
type BibNumber = usize;
type WizFnLoc = usize;
type FieldLoc = usize;
type FnDefLoc = usize;

pub(crate) fn reset_all() {
    log::reset();
    pool::reset();
    history::reset();
    buffer::reset();
    cite::reset();
    auxi::reset();
    bibs::reset();
    hash::reset();
    other::reset();
    entries::reset();
    global::reset();
}

pub(crate) fn bibtex_main(ctx: &mut Bibtex<'_, '_>, aux_file_name: &CStr) -> History {
    reset_all();

    let res = GlobalItems::with(|globals| inner_bibtex_main(ctx, globals, aux_file_name));
    match res {
        Ok(History::Spotless) => (),
        Ok(hist) => return hist,
        Err(BibtexError::Recover) => {
            // SAFETY: bst_file guaranteed valid at this point
            unsafe { peekable_close(ctx, ctx.bst_file) };
            ctx.bst_file = None;
            // SAFETY: bbl_file guaranteed valid at this point
            unsafe { ttbc_output_close(ctx.engine, ctx.bbl_file) };
        }
        Err(BibtexError::NoBst) => {
            // SAFETY: bbl_file guaranteed valid at this point
            unsafe { ttbc_output_close(ctx.engine, ctx.bbl_file) };
        }
        Err(BibtexError::Fatal) => (),
    }

    match get_history() {
        History::Spotless => (),
        History::WarningIssued => {
            if err_count() == 1 {
                write_logs("(There was 1 warning)\n")
            } else {
                write_logs(&format!("(There were {} warnings)\n", err_count()))
            }
        }
        History::ErrorIssued => {
            if err_count() == 1 {
                write_logs("(There was 1 error message)\n")
            } else {
                write_logs(&format!("(There were {} error messages)\n", err_count()))
            }
        }
        History::FatalError => {
            write_logs("(That was a fatal error)\n");
        }
    }

    bib_close_log(ctx);
    get_history()
}

pub(crate) fn inner_bibtex_main(
    ctx: &mut Bibtex<'_, '_>,
    globals: &mut GlobalItems<'_>,
    aux_file_name: &CStr,
) -> Result<History, BibtexError> {
    if !init_standard_output(ctx) {
        return Ok(History::FatalError);
    }

    if initialize(ctx, globals, aux_file_name)? != 0 {
        return Ok(History::FatalError);
    }

    if ctx.config.verbose {
        write_logs("This is BibTeX, Version 0.99d\n");
    } else {
        write_log_file("This is BibTeX, Version 0.99d\n");
    }

    write_log_file(&format!(
        "Capacity: max_strings={}, hash_size={}, hash_prime={}\n",
        pool::MAX_STRINGS,
        hash::HASH_SIZE,
        hash::HASH_PRIME
    ));

    if ctx.config.verbose {
        write_logs("The top-level auxiliary file: ");
        print_aux_name(globals.aux, globals.pool)?;
    } else {
        write_log_file("The top-level auxiliary file: ");
        log_pr_aux_name(globals.aux, globals.pool)?;
    }

    loop {
        globals.aux.set_ln_at_ptr(globals.aux.ln_at_ptr() + 1);

        if !input_ln(Some(globals.aux.file_at_ptr()), globals.buffers) {
            if pop_the_aux_stack(ctx, globals.aux) {
                break;
            }
        } else {
            get_aux_command_and_process(ctx, globals)?;
        }
    }

    last_check_for_aux_errors(ctx, globals.aux, globals.pool, globals.cites, globals.bibs)?;

    if ctx.bst_str == 0 {
        return Err(BibtexError::NoBst);
    }

    ctx.bst_line_num = 0;
    ctx.bbl_line_num = 1;
    globals
        .buffers
        .set_offset(BufTy::Base, 2, globals.buffers.init(BufTy::Base));

    let mut exec = ExecCtx::new(ctx);
    loop {
        if !eat_bst_white_space(exec.glbl_ctx_mut(), globals.buffers) {
            break;
        }
        get_bst_command_and_process(&mut exec, globals)?;
    }

    Ok(History::Spotless)
}

pub(crate) fn get_the_top_level_aux_file_name(
    ctx: &mut Bibtex<'_, '_>,
    GlobalItems {
        pool, hash, aux, ..
    }: &mut GlobalItems<'_>,
    aux_file_name: &CStr,
) -> Result<i32, BibtexError> {
    let ctx = &mut *ctx;
    let aux_bytes = aux_file_name.to_bytes_with_nul();

    // This will be our scratch space for CStr filenames
    let mut path = vec![0; aux_bytes.len()];
    // Copy in all but the presumed trailing `.aux`
    path[..aux_bytes.len() - 5].copy_from_slice(&aux_bytes[..aux_bytes.len() - 5]);

    let set_extension = |path: &mut Vec<_>, extension: &[u8]| {
        let range = path.len() - 5..path.len() - 1;
        path[range].copy_from_slice(extension);
    };

    aux.set_ptr(0);

    let aux_file = match PeekableInput::open(ctx, aux_file_name, FileFormat::Tex) {
        Ok(file) => file,
        Err(_) => {
            sam_wrong_file_name_print(aux_file_name);
            return Ok(1);
        }
    };
    aux.set_file_at_ptr(aux_file);

    set_extension(&mut path, b".blg");
    let log_file = CStr::from_bytes_with_nul(&path).unwrap();
    if !init_log_file(ctx, log_file) {
        sam_wrong_file_name_print(log_file);
        return Ok(1);
    }

    set_extension(&mut path, b".bbl");
    let bbl_file = CStr::from_bytes_with_nul(&path).unwrap();
    // SAFETY: Function sound if provided a valid path pointer
    ctx.bbl_file = unsafe { ttbc_output_open(ctx.engine, bbl_file.as_ptr(), 0) };
    if ctx.bbl_file.is_null() {
        sam_wrong_file_name_print(bbl_file);
        return Ok(1);
    }

    set_extension(&mut path, b".aux");
    let lookup = match pool.lookup_str_insert(hash, &path[..path.len() - 1], StrIlk::AuxFile) {
        Ok(res) => res,
        Err(_) => return Err(BibtexError::Fatal),
    };
    aux.set_at_ptr(hash.text(lookup.loc));

    if lookup.exists {
        write_logs("Already encountered auxiliary file");
        print_confusion();
        return Err(BibtexError::Fatal);
    }

    Ok(0)
}

fn initialize(
    ctx: &mut Bibtex<'_, '_>,
    globals: &mut GlobalItems<'_>,
    aux_file_name: &CStr,
) -> Result<i32, BibtexError> {
    globals.pool.set_pool_ptr(0);
    globals.pool.set_str_ptr(1);
    globals.pool.set_start(globals.pool.str_ptr(), 0);

    ctx.bib_seen = false;
    ctx.bst_seen = false;
    ctx.citation_seen = false;
    ctx.all_entries = false;

    ctx.entry_seen = false;
    ctx.read_seen = false;
    ctx.read_performed = false;
    ctx.reading_completed = false;
    ctx.impl_fn_num = 0;
    globals.buffers.set_init(BufTy::Out, 0);

    pre_def_certain_strings(ctx, globals)?;
    get_the_top_level_aux_file_name(ctx, globals, aux_file_name)
}

mod external {
    use super::*;

    #[allow(improper_ctypes)]
    extern "C" {
        pub(crate) fn ttbc_input_open(
            engine: *mut CoreBridgeState<'_>,
            path: *const libc::c_char,
            format: FileFormat,
            is_gz: libc::c_int,
        ) -> *mut InputHandle;
        pub(crate) fn ttbc_input_close(
            engine: *mut CoreBridgeState<'_>,
            input: *mut InputHandle,
        ) -> libc::c_int;
        pub(crate) fn ttbc_output_open_stdout(
            engine: *mut CoreBridgeState<'_>,
        ) -> *mut OutputHandle;
        pub(crate) fn ttbc_output_open(
            engine: *mut CoreBridgeState<'_>,
            path: *const libc::c_char,
            is_gz: libc::c_int,
        ) -> *mut OutputHandle;
        pub(crate) fn ttbc_output_close(
            engine: *mut CoreBridgeState<'_>,
            handle: *mut OutputHandle,
        ) -> libc::c_int;

        pub(crate) fn xrealloc(ptr: *mut libc::c_void, size: libc::size_t) -> *mut libc::c_void;

        pub(crate) fn xcalloc(elems: libc::size_t, elem_size: libc::size_t) -> *mut libc::c_void;
    }
}

/// Does our resulting executable link correctly?
#[test]
fn linkage() {}
