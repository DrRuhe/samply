use pdb::{FallibleIterator, Result, SymbolData, PDB};
use pdb_addr2line::TypeFormatter;
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Frame<'s> {
    pub function: Option<String>,
    pub location: Option<Location<'s>>,
}

#[derive(Clone)]
pub struct Location<'s> {
    pub file: Option<std::borrow::Cow<'s, str>>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

pub struct Addr2LineContext<'a, 's>
where
    's: 'a,
{
    address_map: &'a pdb::AddressMap<'s>,
    string_table: &'a pdb::StringTable<'s>,
    dbi: &'a pdb::DebugInformation<'s>,
    type_formatter: &'a TypeFormatter<'a>,
}

impl<'a, 's> Addr2LineContext<'a, 's> {
    pub fn new(
        address_map: &'a pdb::AddressMap<'s>,
        string_table: &'a pdb::StringTable<'s>,
        dbi: &'a pdb::DebugInformation<'s>,
        type_formatter: &'a TypeFormatter<'a>,
    ) -> Result<Self> {
        Ok(Self {
            address_map,
            string_table,
            dbi,
            type_formatter,
        })
    }

    pub fn find_frames<'b, 't, S>(
        &self,
        pdb: &mut PDB<'t, S>,
        address: u32,
    ) -> Result<Vec<Frame<'b>>>
    where
        S: pdb::Source<'t>,
        's: 't,
        S: 's,
        's: 'b,
        'a: 'b,
    {
        let mut modules = self.dbi.modules()?.filter_map(|m| pdb.module_info(&m));
        while let Some(module_info) = modules.next()? {
            let proc_symbol = module_info.symbols()?.find_map(|symbol| {
                if let Ok(SymbolData::Procedure(proc)) = symbol.parse() {
                    let start_rva = match proc.offset.to_rva(&self.address_map) {
                        Some(rva) => rva,
                        None => return Ok(None),
                    };

                    let procedure_rva_range = start_rva.0..(start_rva.0 + proc.len);
                    if !procedure_rva_range.contains(&address) {
                        return Ok(None);
                    }
                    return Ok(Some((symbol.index(), proc, procedure_rva_range)));
                }
                Ok(None)
            })?;

            if let Some((symbol_index, proc, procedure_rva_range)) = proc_symbol {
                let line_program = module_info.line_program()?;

                let inlinees: BTreeMap<pdb::IdIndex, pdb::Inlinee> = module_info
                    .inlinees()?
                    .map(|i| Ok((i.index(), i)))
                    .collect()?;

                return self.find_frames_from_procedure(
                    address,
                    &module_info,
                    symbol_index,
                    proc,
                    procedure_rva_range,
                    &line_program,
                    &inlinees,
                );
            }
        }
        Ok(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn find_frames_from_procedure<'b>(
        &self,
        address: u32,
        module_info: &pdb::ModuleInfo,
        symbol_index: pdb::SymbolIndex,
        proc: pdb::ProcedureSymbol,
        procedure_rva_range: std::ops::Range<u32>,
        line_program: &pdb::LineProgram,
        inlinees: &BTreeMap<pdb::IdIndex, pdb::Inlinee>,
    ) -> Result<Vec<Frame<'b>>>
    where
        's: 'b,
        'a: 'b,
    {
        let mut formatted_function_name = String::new();
        let _ = self.type_formatter.write_function(
            &mut formatted_function_name,
            &proc.name.to_string(),
            proc.type_index,
        );
        let function = Some(formatted_function_name);

        // Ordered outside to inside, until just before the end of this function.
        let mut frames_per_address: BTreeMap<u32, Vec<_>> = BTreeMap::new();

        let frame = Frame {
            function,
            location: None,
        };
        frames_per_address.insert(address, vec![frame]);

        let lines_for_proc = line_program.lines_at_offset(proc.offset);
        if let Some(line_info) = self.find_line_info_containing_address_no_size(
            lines_for_proc,
            address,
            procedure_rva_range.end,
        ) {
            let location = self.line_info_to_location(line_info, &line_program);
            let frame = &mut frames_per_address.get_mut(&address).unwrap()[0];
            frame.location = Some(location.clone());
        }

        let mut inline_symbols_iter = module_info.symbols_at(symbol_index)?;

        // Skip the procedure symbol that we're currently in.
        inline_symbols_iter.next()?;

        while let Some(symbol) = inline_symbols_iter.next()? {
            match symbol.parse() {
                Ok(SymbolData::Procedure(_)) => {
                    // This is the start of the procedure *after* the one we care about. We're done.
                    break;
                }
                Ok(SymbolData::InlineSite(site)) => {
                    if let Some(frame) = self.frames_for_address_for_inline_symbol(
                        site,
                        address,
                        &inlinees,
                        proc.offset,
                        &line_program,
                    ) {
                        frames_per_address
                            .get_mut(&address)
                            .unwrap()
                            .push(frame.clone());
                    }
                }
                _ => {}
            }
        }

        // Now order from inside to outside.
        for (_address, frames) in frames_per_address.iter_mut() {
            frames.reverse();
        }

        Ok(frames_per_address.into_iter().next().unwrap().1)
    }

    fn frames_for_address_for_inline_symbol<'b>(
        &self,
        site: pdb::InlineSiteSymbol,
        address: u32,
        inlinees: &BTreeMap<pdb::IdIndex, pdb::Inlinee>,
        proc_offset: pdb::PdbInternalSectionOffset,
        line_program: &pdb::LineProgram,
    ) -> Option<Frame<'b>>
    where
        's: 'b,
        'a: 'b,
    {
        // This inlining site only covers the address if it has a line info that covers this address.
        let inlinee = inlinees.get(&site.inlinee)?;
        let lines = inlinee.lines(proc_offset, &site);
        let line_info = match self.find_line_info_containing_address_with_size(lines, address) {
            Some(line_info) => line_info,
            None => return None,
        };

        let mut formatted_name = String::new();
        let _ = self
            .type_formatter
            .write_id(&mut formatted_name, site.inlinee);
        let function = Some(formatted_name);

        let location = self.line_info_to_location(line_info, line_program);

        Some(Frame {
            function,
            location: Some(location),
        })
    }

    fn find_line_info_containing_address_no_size(
        &self,
        iterator: impl FallibleIterator<Item = pdb::LineInfo, Error = pdb::Error> + Clone,
        address: u32,
        outer_end_rva: u32,
    ) -> Option<pdb::LineInfo> {
        let start_rva_iterator = iterator
            .clone()
            .map(|line_info| Ok(line_info.offset.to_rva(&self.address_map).unwrap().0));
        let outer_end_rva_iterator = fallible_once(Ok(outer_end_rva));
        let end_rva_iterator = start_rva_iterator
            .clone()
            .skip(1)
            .chain(outer_end_rva_iterator);
        let mut line_iterator = start_rva_iterator.zip(end_rva_iterator).zip(iterator);
        while let Ok(Some(((start_rva, end_rva), line_info))) = line_iterator.next() {
            if start_rva <= address && address < end_rva {
                return Some(line_info);
            }
        }
        None
    }

    fn find_line_info_containing_address_with_size(
        &self,
        mut iterator: impl FallibleIterator<Item = pdb::LineInfo, Error = pdb::Error> + Clone,
        address: u32,
    ) -> Option<pdb::LineInfo> {
        while let Ok(Some(line_info)) = iterator.next() {
            let length = match line_info.length {
                Some(l) => l,
                None => continue,
            };
            let start_rva = line_info.offset.to_rva(&self.address_map).unwrap().0;
            let end_rva = start_rva + length;
            if start_rva <= address && address < end_rva {
                return Some(line_info);
            }
        }
        None
    }

    fn line_info_to_location<'b>(
        &self,
        line_info: pdb::LineInfo,
        line_program: &pdb::LineProgram,
    ) -> Location<'b>
    where
        'a: 'b,
        's: 'b,
    {
        let file = line_program
            .get_file_info(line_info.file_index)
            .and_then(|file_info| file_info.name.to_string_lossy(&self.string_table))
            .ok();
        Location {
            file,
            line: Some(line_info.line_start),
            column: line_info.column_start,
        }
    }
}

fn fallible_once<T, E>(value: std::result::Result<T, E>) -> Once<T, E> {
    Once { value: Some(value) }
}

struct Once<T, E> {
    value: Option<std::result::Result<T, E>>,
}

impl<T, E> FallibleIterator for Once<T, E> {
    type Item = T;
    type Error = E;

    fn next(&mut self) -> std::result::Result<Option<Self::Item>, Self::Error> {
        match self.value.take() {
            Some(Ok(value)) => Ok(Some(value)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}
