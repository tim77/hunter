use std::fmt::Debug;
use std::path::{Path, PathBuf};

use termion::event::Key;
use unicode_width::UnicodeWidthStr;
use rayon::prelude::*;

use async_value::{Stale, StopIter};

use crate::files::{File, Files};
use crate::fail::{HResult, HError, ErrorLog};
use crate::term;
use crate::widget::{Widget, WidgetCore};
use crate::dirty::Dirtyable;
use crate::fscache::FsCache;


pub trait Listable {
    type Item: Debug + PartialEq + Default;
    fn len(&self) -> usize;
    fn render(&self) -> Vec<String>;
    fn render_header(&self) -> HResult<String> { Ok("".to_string()) }
    fn render_footer(&self) -> HResult<String> { Ok("".to_string()) }
    fn on_new(&mut self) -> HResult<()> { Ok(()) }
    fn on_refresh(&mut self) -> HResult<()> { Ok(()) }
    fn on_key(&mut self, _key: Key) -> HResult<()> { Ok(()) }
}

use crate::keybind::{Acting, Bindings, FileListAction, Movement};


impl Acting for ListView<Files> {
    type Action=FileListAction;

    fn search_in(&self) -> Bindings<Self::Action> {
        self.core.config().keybinds.filelist
    }

    fn movement(&mut self, movement: &Movement) -> HResult<()> {
        use Movement::*;

        let pos = self.get_selection();

        match movement {
            Up(n) => { for _ in 0..*n { self.move_up(); }; self.refresh()?; }
            Down(n) => { for _ in 0..*n { self.move_down(); }; self.refresh()?; }
            PageUp => self.page_up(),
            PageDown => self.page_down(),
            Top => self.move_top(),
            Bottom => self.move_bottom(),
            Left | Right => {}
        }

        if pos != self.get_selection() {
            self.update_selected_file();
        }

        Ok(())
    }

    fn do_action(&mut self, action: &Self::Action) -> HResult<()> {
        use FileListAction::*;

        let pos = self.get_selection();

        match action {
            Search => self.search_file()?,
            SearchNext => self.search_next()?,
            SearchPrev => self.search_prev()?,
            Filter => self.filter()?,
            Select => self.multi_select_file(),
            InvertSelection => self.invert_selection(),
            ClearSelection => self.clear_selections(),
            FilterSelection => self.toggle_filter_selected(),
            ToggleTag => self.toggle_tag()?,
            ToggleHidden => self.toggle_hidden(),
            ReverseSort => self.reverse_sort(),
            CycleSort => self.cycle_sort(),
            ToNextMtime => self.select_next_mtime(),
            ToPrevMtime => self.select_prev_mtime(),
            ToggleDirsFirst => self.toggle_dirs_first(),
        }

        if pos != self.get_selection() {
            self.update_selected_file();
        }

        Ok(())
    }
}

impl Listable for ListView<Files> {
    type Item = File;

    fn len(&self) -> usize {
        self.content.len()
    }

    fn render(&self)-> Vec<String> {
        self.render()
    }

    fn on_new(&mut self) -> HResult<()> {
        let show_hidden = self.core.config().show_hidden();
        self.content.show_hidden = show_hidden;
        let mut file = self.content
            .iter_files()
            .nth(0)
            .cloned()
            .unwrap_or_default();

        if file.meta.is_none() {
            file.meta_sync().log();
        }

        self.current_item = Some(file);
        Ok(())
    }

    fn on_refresh(&mut self) -> HResult<()> {
        if self.content.len() == 0 {
            let path = &self.content.directory.path;
            let placeholder = File::new_placeholder(&path)?;
            self.content.files.push(placeholder);
            self.content.len = 1;
        }

        self.refresh_files().log();

        if self.content.is_dirty() {
            self.content.set_clean();
            self.core.set_dirty();
        }

        Ok(())
    }

    fn on_key(&mut self, key: Key) -> HResult<()> {
        self.do_key(key)
    }
}

#[derive(Debug, PartialEq)]
pub struct ListView<T>
where
    ListView<T>: Listable
{
    pub content: T,
    pub current_item: Option<<ListView<T> as Listable>::Item>,
    // pub lines: usize,
    selection: usize,
    pub offset: usize,
    //pub buffer: Vec<String>,
    pub core: WidgetCore,
    seeking: bool,
    searching: Option<String>,
}

impl<T> ListView<T>
where
    ListView<T>: Widget,
    ListView<T>: Listable
{
    pub fn new(core: &WidgetCore, content: T) -> ListView<T> {
        let mut view = ListView::<T> {
            content: content,
            current_item: None,
            // lines: 0,
            selection: 0,
            offset: 0,
            // buffer: Vec::new(),
            core: core.clone(),
            seeking: false,
            searching: None
        };
        view.on_new().log();
        view
    }

    pub fn move_up(&mut self) {
        if self.selection == 0 {
            return;
        }

        if self.selection - self.offset <= 0 {
            self.offset -= 1;
        }

        self.selection -= 1;
        self.seeking = false;
    }
    pub fn move_down(&mut self) {
        let lines = self.len();
        let y_size = self.get_coordinates().unwrap().ysize() as usize;

        if lines == 0 || self.selection == lines - 1 {
            return;
        }

        if self.selection + 1 >= y_size && self.selection + 1 - self.offset >= y_size {
            self.offset += 1;
        }

        self.selection += 1;
        self.seeking = false;
    }

    pub fn move_top(&mut self) {
        self.set_selection(0);
    }

    pub fn move_bottom(&mut self) {
        let lines = self.len();
        self.set_selection(lines - 1);
    }

    pub fn page_up(&mut self) {
        let ysize = self.get_coordinates().unwrap().ysize_u();

        for _ in 0..ysize {
            self.move_up();
        }
    }

    pub fn page_down(&mut self) {
        let ysize = self.get_coordinates().unwrap().ysize_u();

        for _ in 0..ysize {
            self.move_down();
        }
    }

    pub fn get_selection(&self) -> usize {
        self.selection
    }

    pub fn set_selection(&mut self, position: usize) {
        let ysize = self.get_coordinates().unwrap().ysize() as usize;
        let mut offset = 0;

        while position >= ysize + offset {
            offset += 1
        }

        self.offset = offset;
        self.selection = position;
    }

}

pub enum FileSource {
    Path(File),
    Files(Files)
}


pub struct FileListBuilder {
    core: WidgetCore,
    source: FileSource,
    cache: Option<crate::fscache::FsCache>,
    selected_file: Option<File>,
    stale: Option<Stale>,
    meta_upto: usize,
    meta_all: bool,
    prerender: bool
}

impl FileListBuilder {
    pub fn new(core: WidgetCore, source: FileSource) -> Self {
        FileListBuilder {
            core: core,
            source: source,
            cache: None,
            selected_file: None,
            stale: None,
            meta_upto: 0,
            meta_all: false,
            prerender: false
        }
    }

    pub fn select(mut self, file: impl Into<Option<File>>) -> Self {
        self.selected_file = file.into();
        self
    }

    pub fn with_cache(mut self, cache: impl Into<Option<FsCache>>) -> Self {
        self.cache = cache.into();
        self
    }

    pub fn with_stale(mut self, stale: impl Into<Option<Stale>>) -> Self {
        self.stale = stale.into();
        self
    }

    pub fn meta_upto(mut self, upto: impl Into<Option<usize>>) -> Self {
        self.meta_upto = upto.into().unwrap_or(0);
        self
    }

    pub fn meta_all(mut self) -> Self {
        self.meta_all = true;
        self
    }

    pub fn prerender(mut self) -> Self {
        self.prerender = true;
        self
    }

    pub fn build(self) -> HResult<ListView<Files>> {
        let c = &self.cache;
        let s = self.stale.clone();
        let files = match self.source {
            FileSource::Files(f) => Ok(f),
            FileSource::Path(f) => {
                c.as_ref()
                 .map_or_else(| | Files::new_from_path(&f.path),
                              |c| s.map_or_else(| | c.get_files_sync(&f),
                                                |s| c.get_files_sync_stale(&f, s)))
            }
        }?;

        let mut view = ListView::new(&self.core, files);

        let selected_file = match self.selected_file {
            Some(f) => Some(f),
            None => {
                c.as_ref()
                 .map(|c| c.get_selection(&view.content.directory).ok())
                 .flatten()
            }
        };

        selected_file.map(|mut f| {
            f.meta_sync().log();
            view.select_file(&f);
        });

        let from = match self.meta_all {
            true => 0,
            false => view.offset,
        };

        let ysize = view.core.coordinates.ysize_u();
        let upto = match self.meta_all {
            true => view.content.len,
            false => from + ysize + 1
        };

        view.content
            .iter_files_mut()
            .skip(from)
            .take(upto)
            .par_bridge()
            .for_each(|f| {
                f.meta_sync().log();
                if f.is_dir() {
                    f.run_dirsize();
                }
            });
        view.content.meta_upto = Some(upto);

        // if self.prerender {
        //     match self.stale {
        //         Some(s) => view.render_buffer_stale(s)?,
        //         None => view.render_buffer()?
        //     }

        //     if view.buffer.len() > 0 {
        //         view.lines = view.buffer.len() - 1;
        //     }
        // };

        view.content.set_clean();
        // view.content.dirty_meta.set_clean();
        view.core.set_clean();

        Ok(view)
    }
}

impl ListView<Files>
{
    pub fn builder(core: WidgetCore, source: FileSource) -> FileListBuilder {
        FileListBuilder::new(core, source)
    }
    pub fn update_selected_file(&mut self) {
        let pos = self.selection;

        let file = self.content
            .iter_files()
            .nth(pos)
            .map(|f| f.clone());

        self.current_item = file;
    }

    pub fn selected_file(&self) -> &File {
        self.current_item.as_ref().unwrap()
    }

    pub fn selected_file_mut(&mut self) -> &mut File {
        let selection = self.selection;

        let file = self.content
            .iter_files_mut()
            .nth(selection)
            .map(|f| f as *mut File);


        // Work around annoying restriction until polonius borrow checker becomes default
        // Since only ever one mutable borrow is returned this is perfectly safe
        // See also: https://github.com/rust-lang/rust/issues/21906
        match file {
            Some(file) => unsafe { return file.as_mut().unwrap() },
            None => {
                &mut self.content.directory
            }
        }
    }

    pub fn clone_selected_file(&self) -> File {
        let file = self.selected_file().clone();
        file
    }

    pub fn grand_parent(&self) -> Option<PathBuf> {
        self.selected_file().grand_parent()
    }

    pub fn goto_grand_parent(&mut self) -> HResult<()> {
        match self.grand_parent() {
            Some(grand_parent) => self.goto_path(&grand_parent),
            None => { self.core.show_status("Can't go further!") },
        }
    }

    fn goto_selected(&mut self) -> HResult<()> {
        let path = self.selected_file().path();

        self.goto_path(&path)
    }

    pub fn goto_path(&mut self, path: &Path) -> HResult<()> {
        match crate::files::Files::new_from_path(path) {
            Ok(files) => {
                self.content = files;
                self.selection = 0;
                self.offset = 0;
                self.refresh()
            }
            Err(err) => {
                self.core.show_status(&format!("Can't open this path: {}", err))
            }
        }
    }

    pub fn select_file(&mut self, file: &File) {
        let file = file.clone();
        self.current_item = Some(file);

        let pos = self
            .content
            .iter_files()
            .position(|item| item == self.selected_file())
            .unwrap_or(0);
        self.set_selection(pos);
    }

    fn cycle_sort(&mut self) {
        let file = self.clone_selected_file();
        self.content.cycle_sort();
        self.content.sort();
        self.select_file(&file);
        self.refresh().log();
        self.core.show_status(&format!("Sorting by: {}", self.content.sort)).log();
    }

    fn reverse_sort(&mut self) {
        let file = self.clone_selected_file();
        self.content.reverse_sort();
        self.content.sort();
        self.select_file(&file);
        self.refresh().log();
        self.core.show_status(&format!("Reversed sorting by: {}",
                                       self.content.sort)).log();
    }

    fn select_next_mtime(&mut self) {
        let file = self.clone_selected_file();
        let dir_settings = self.content.dirs_first;
        let sort_settings = self.content.sort;

        self.content.dirs_first = false;
        self.content.sort = crate::files::SortBy::MTime;
        self.content.sort();

        self.select_file(&file);

        if self.seeking == false || self.selection + 1 >= self.content.len() {
            self.selection = 0;
            self.offset = 0;
        } else {
            self.move_down();
         }

        let file = self.clone_selected_file();
        self.content.dirs_first = dir_settings;
        self.content.sort = sort_settings;
        self.content.sort();
        self.select_file(&file);
        self.seeking = true;

        self.refresh().log();
    }

    fn select_prev_mtime(&mut self) {
        let file = self.clone_selected_file();
        let dir_settings = self.content.dirs_first;
        let sort_settings = self.content.sort;

        self.content.dirs_first = false;
        self.content.sort = crate::files::SortBy::MTime;
        self.content.sort();

        self.select_file(&file);

        if self.seeking == false || self.selection == 0 {
            self.set_selection(self.content.len() - 1);
        } else {
            self.move_up();
        }

        let file = self.clone_selected_file();
        self.content.dirs_first = dir_settings;
        self.content.sort = sort_settings;
        self.content.sort();
        self.select_file(&file);
        self.seeking = true;

        self.refresh().log();
    }

    pub fn toggle_hidden(&mut self) {
        let file = self.clone_selected_file();
        self.content.toggle_hidden();
        self.select_file(&file);
        self.refresh().log();
        self.core.show_status(&format!("Showing hidden files: {}",
                                        self.content.show_hidden)).log();
    }

    fn toggle_dirs_first(&mut self) {
        let file = self.clone_selected_file();
        self.content.dirs_first = !self.content.dirs_first;
        self.content.sort();
        self.select_file(&file);
        self.refresh().log();
        self.core.show_status(&format!("Direcories first: {}",
                                        self.content.dirs_first)).log();
    }

    fn multi_select_file(&mut self) {
        self.selected_file_mut().toggle_selection();

        // Create mutable clone to render change
        let mut file = self.clone_selected_file();
        file.toggle_selection();

        if !self.content.filter_selected {
            //let selection = self.get_selection();
            //let line = self.render_line(&file);
            //self.buffer[selection] = line;

            self.move_down();
        } else {
            if self.content.filter_selected && self.content.len() == 0 {
                self.content.toggle_filter_selected();
                self.core.show_status("Disabled selection filter!").log();
            }

            // fix cursor when last file is unselected, etc
            self.refresh().log();
        }
    }

    pub fn invert_selection(&mut self) {
        for file in self.content.iter_files_mut() {
            file.toggle_selection();
        }

        if self.content.filter_selected && self.content.len() == 0 {
                self.content.toggle_filter_selected();
                self.core.show_status("Disabled selection filter!").log();
        }

        self.content.set_dirty();
        self.refresh().log();
    }

    pub fn clear_selections(&mut self) {
        for file in self.content.iter_files_mut() {
            file.selected = false;
        }
        self.content.set_dirty();
        self.refresh().log();
    }

    fn toggle_tag(&mut self) -> HResult<()> {
        self.selected_file_mut().toggle_tag()?;

        // Create a mutable clone to render changes into buffer
        // let mut file = self.clone_selected_file();
        // file.toggle_tag()?;

        // let line = self.render_line(&file);
        // let selection = self.get_selection();
        // self.buffer[selection] = line;

        self.move_down();
        Ok(())
    }

    fn search_file(&mut self) -> HResult<()> {
        let selected_file = self.clone_selected_file();

        loop {
            let input = self.core.minibuffer_continuous("search");

            match input {
                Ok(input) => {
                    // Only set this, search is on-the-fly
                    self.searching = Some(input);
                }
                Err(HError::MiniBufferInputUpdated(input)) => {
                    let file = self.content
                        .find_file_with_name(&input)
                        .cloned();

                    file.map(|f| self.select_file(&f));

                    self.draw().log();

                    continue;
                },
                Err(HError::MiniBufferEmptyInput) |
                Err(HError::MiniBufferCancelledInput) => {
                    self.select_file(&selected_file);
                }
                _ => {  }
            }
            break;
        }
        Ok(())
    }

    fn search_next(&mut self) -> HResult<()> {
        if self.searching.is_none() {
            self.core.show_status("No search pattern set!").log();
        }
        let prev_search = self.searching.clone()?;
        let selection = self.get_selection();

        let file = self.content
            .files
            .iter()
            .skip(selection+1)
            .find(|file| {
                if file.name.to_lowercase().contains(&prev_search) {
                    true
                } else {
                    false
                }
            }).clone();

        if let Some(file) = file {
            let file = file.clone();
            self.select_file(&file);
        } else {
            self.core.show_status("Reached last search result!").log();
        }
        Ok(())
    }

    fn search_prev(&mut self) -> HResult<()> {
        if self.searching.is_none() {
            self.core.show_status("No search pattern set!").log();
        }
        let prev_search = self.searching.clone()?;


        self.reverse_sort();

        let selection = self.get_selection();

        let file = self.content
            .files
            .iter()
            .skip(selection+1)
            .find(|file| {
                if file.name.to_lowercase().contains(&prev_search) {
                    true
                } else {
                    false
                }
            }).cloned();

        self.reverse_sort();
        self.core.clear_status().log();

        if let Some(file) = file {
            let file = file.clone();
            self.select_file(&file);
        } else {
            self.core.show_status("Reached last search result!").log();
        }

        Ok(())
    }

    fn filter(&mut self) -> HResult<()> {
        let selected_file = self.selected_file().clone();

        loop {
            let filter = self.core.minibuffer_continuous("filter");

            match filter {
                Err(HError::MiniBufferInputUpdated(input)) => {
                    self.content.set_filter(Some(input));
                    self.refresh().ok();

                    self.select_file(&selected_file);
                    self.draw().ok();

                    continue;
                }
                Err(HError::MiniBufferEmptyInput) |
                Err(HError::MiniBufferCancelledInput) => {
                    self.content.set_filter(None);
                    self.refresh().ok();
                    self.select_file(&selected_file);
                }
                _ => {}
            }

            let msgstr = filter.clone().unwrap_or(String::from(""));
            self.core.show_status(&format!("Filtering with: \"{}\"", msgstr)).log();

            break;
        }

        Ok(())
    }

    fn toggle_filter_selected(&mut self) {
        self.content.toggle_filter_selected();

        if self.content.len() == 0 {
            self.core.show_status("No files selected").log();
            self.content.toggle_filter_selected();
        }

        self.refresh().log();
    }

    fn render_line(&self, file: &File) -> String {
        let render_fn = self.render_line_fn();
        render_fn(file)
    }

    #[allow(trivial_bounds)]
    fn render_line_fn(&self) -> impl Fn(&File) -> String {
        use std::fmt::Write;
        let xsize = self.get_coordinates().unwrap().xsize();
        let icons = self.core.config().icons;

        move |file| -> String {
            let mut line = String::with_capacity(500);

            let icon = match icons {
                true => file.icon(),
                false => ""
            };

            let name = &file.name;

            let size = file.calculate_size();
            let (size, unit) = match size {
                Ok((size, unit)) => (size, unit),
                Err(_) => (0 as u32, "")
            };

            let (tag, tag_len) = match file.is_tagged() {
                Ok(true) => (Some(term::color_red() + "*"), 1),
                _ => (None, 0)
            };

            let tag = tag.as_ref()
                         .map(|t| t.as_str())
                         .unwrap_or("");

            let selection_color = crate::term::color_yellow();
            let (selection_gap, selection_color) = match file.is_selected() {
                true => (" ", selection_color.as_str()),
                false => ("", "")
            };

            let (link_indicator, link_indicator_len) = match file.target {
                Some(_) => (Some(format!("{}{}{}",
                                         term::color_yellow(),
                                         "--> ",
                                         term::highlight_color())), Some(4)),
                None => (None, None)
            };

            let link_indicator = link_indicator.as_ref()
                                               .map(|l| l.as_str())
                                               .unwrap_or("");
            let link_indicator_len = link_indicator_len.unwrap_or(0);

            let sized_string = term::sized_string(&name, xsize);

            let size = size.to_string();
            let size_pos = xsize - (size.len() as u16 +
                                    unit.len() as u16 +
                                    link_indicator_len as u16);

            let padding = sized_string.len() - sized_string.width_cjk();
            let padding = xsize - padding as u16;
            let padding = padding - tag_len;
            let padding = padding - icon.width() as u16;

            write!(&mut line, "{}", termion::cursor::Save).unwrap();

            match &file.color {
                Some(color) => write!(&mut line,
                                      "{}{}{}{}{}{:padding$}{}",
                                      tag,
                                      term::from_lscolor(color),
                                      selection_color,
                                      selection_gap,
                                      icon,
                                      &sized_string,
                                      term::normal_color(),
                                      padding = padding as usize),
                None => write!(&mut line,
                               "{}{}{}{}{}{:padding$}{}",
                               tag,
                               term::normal_color(),
                               selection_color,
                               selection_gap,
                               icon,
                               &sized_string,
                               term::normal_color(),
                               padding = padding as usize),
            }.unwrap();

            write!(&mut line,
                   "{}{}{}{}{}{}",
                   termion::cursor::Restore,
                   termion::cursor::Right(size_pos),
                   link_indicator,
                   term::highlight_color(),
                   size,
                   unit).unwrap();


            line
        }
    }


    fn render(&self) -> Vec<String> {
        let render_fn = self.render_line_fn();
        let ysize = self.get_coordinates().unwrap().ysize_u();
        self.content
            .iter_files()
            .skip(self.offset)
            .take(ysize+1)
            // .collect::<Vec<_>>()
            // .into_par_iter()
            .map(|file| render_fn(file))
            .collect()
    }

    fn render_buffer(&mut self) -> HResult<()> {
        // let render_fn = self.render_line_fn();
        // self.buffer = self.content
        //                  .iter_files()
        //                  .enumerate()
        //                  .map(|(_, file)| {
        //                      render_fn(file)
        //                  })
        //                  .collect();
        Ok(())
    }

    fn render_buffer_stale(&mut self, stale: Stale) -> HResult<()> {
        // let render_fn = self.render_line_fn();
        // let buffer = self.content
        //                  .iter_files()
        //                  .stop_stale(stale.clone())
        //                  .enumerate()
        //                  .map(|(_, file)| {
        //                      render_fn(file)
        //                  })
        //                  .collect();

        // if stale.is_stale()
        //         .unwrap_or(true) {
        //             return HError::stale();
        //         } else {
        //             self.buffer = buffer;
        //             return Ok(())
        //         }
        Ok(())
    }

    fn refresh_files(&mut self) -> HResult<()> {
        // if let Ok(Some(mut refresh)) = self.content.get_refresh() {
        //     let file = self.clone_selected_file();

        //     self.buffer = refresh.new_buffer.take()?;
        //     self.lines = self.buffer.len() - 1;

        //     self.select_file(&file);
        // }

        // if self.content.ready_to_refresh()? {
        //     let render_fn = self.render_line_fn();
        //     self.content.process_fs_events(self.buffer.clone(),
        //                                    self.core.get_sender(),
        //                                    render_fn)?;
        // }

        Ok(())
    }
}


impl<T> Widget for ListView<T>
where
    ListView<T>: Listable
{
    fn get_core(&self) -> HResult<&WidgetCore> {
        Ok(&self.core)
    }
    fn get_core_mut(&mut self) -> HResult<&mut WidgetCore> {
        Ok(&mut self.core)
    }
    fn refresh(&mut self) -> HResult<()> {
        self.on_refresh().log();

        // let buffer_len = self.buffer.len();

        // self.lines = buffer_len;

        if self.selection >= self.len() && self.len() != 0 {
            self.selection = self.len() - 1;
        }

        // if self.core.is_dirty() {
        //     self.buffer = self.render();
        //     self.core.set_clean();
        // }
        Ok(())
    }

    fn render_header(&self) -> HResult<String> {
        Listable::render_header(self)
    }

    fn render_footer(&self) -> HResult<String> {
        Listable::render_footer(self)
    }

    fn get_drawlist(&self) -> HResult<String> {
        let mut output = term::reset();
        let (xpos, ypos) = self.get_coordinates().unwrap().position().position();

        let render = self.render();

        output += &render
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let mut output = term::normal_color();

                // i counts from the offset, while selection counts from 0
                if i + self.offset == self.selection {
                    output += &term::invert();
                }

                output += &format!(
                    "{}{}{}",
                    term::goto_xy(xpos, i as u16 + ypos),
                    item,
                    term::reset()
                );
                String::from(output)
            })
            .collect::<String>();

        output += &self.get_redraw_empty_list(self.len())?;

        Ok(output)
    }

    fn on_key(&mut self, key: Key) -> HResult<()> {
        Listable::on_key(self, key)
    }
}
