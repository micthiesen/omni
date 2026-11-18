//! The tabular chart: one row per input, each step a cell spanning the rows
//! of everything it combines, reading left to right to the finished dish.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::index::{Index, Kind};
use crate::list::make_ahead_note;
use crate::model::{Id, Input, RecipeDoc, Step};
use crate::py;
use crate::text::{chart_label, use_label};

/// `leaf` is one use of an ingredient; `setup` is a step with no inputs, such
/// as heating the oven; `ref` is a by-product, an earlier component, or a
/// result drawn in its own block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    Leaf,
    Step,
    Setup,
    Ref,
}

impl CellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::Step => "step",
            Self::Setup => "setup",
            Self::Ref => "ref",
        }
    }
}

/// One cell, covering rows `r0..=r1` and columns `c0..=c1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cell {
    /// The step id for a step or setup cell; `step/ref` or `step/ref:part`
    /// for an input row.
    pub key: String,
    pub label: String,
    pub kind: CellKind,
    pub r0: usize,
    pub r1: usize,
    pub c0: usize,
    pub c1: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// The step the cell belongs to; for an input row, the consuming step.
    pub step: Id,
}

/// One table: a component, or a step whose result is split between several
/// steps. Its cells tile `rows` × `cols` exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Block {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// "make ahead: keeps 3 days in the fridge".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub cells: Vec<Cell>,
    pub rows: usize,
    pub cols: usize,
}

/// `ready`: every step this one waits for is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Mark {
    Done,
    Ready,
}

impl Mark {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Ready => "▶",
        }
    }
}

/// Cook-mode marks by cell key. A step id marks the step's cell and, when
/// done, its input rows; an input row's own key marks that row alone.
pub type Marks = BTreeMap<String, Mark>;

/// A chain of single-use steps deeper than this is cut into reference rows,
/// which bounds recursion on documents the validator would refuse.
const DEPTH_MAX: usize = 256;

/// One block per component, plus one per step whose result is split between
/// several steps. A component without steps has no block.
pub fn chart_blocks(doc: &RecipeDoc, factor: f64) -> Vec<Block> {
    let ix = Index::build(doc);
    let shared: HashSet<&str> = ix
        .consumed()
        .iter()
        .copied()
        .filter(|id| ix.kind(id) == Some(Kind::Step) && ix.consumers(id).len() > 1)
        .collect();
    let multi = doc.components.len() > 1;
    let mut blocks = Vec::new();
    for component in &doc.components {
        for step in &component.steps {
            if shared.contains(step.id.as_str()) {
                let name = step.output.as_deref().unwrap_or(&step.id);
                let title = Some(py::capitalize(name));
                blocks.push(Builder::new(&ix, &shared, factor).block(step, title, None));
            }
        }
        if let Some(root) = component.steps.last() {
            let title = component.name.clone().filter(|_| multi);
            let note = make_ahead_note(component);
            blocks.push(Builder::new(&ix, &shared, factor).block(root, title, note));
        }
    }
    if blocks.len() > 1
        && let Some(last) = blocks.last_mut()
        && last.title.is_none()
    {
        last.title = Some(doc.title.clone());
    }
    blocks
}

struct Builder<'a, 'd> {
    ix: &'a Index<'d>,
    shared: &'a HashSet<&'d str>,
    factor: f64,
    cells: Vec<Cell>,
    row: usize,
    /// Steps being drawn, root first; a step already on the path is drawn as
    /// a reference row.
    path: Vec<&'d str>,
}

impl<'a, 'd> Builder<'a, 'd> {
    fn new(ix: &'a Index<'d>, shared: &'a HashSet<&'d str>, factor: f64) -> Self {
        Self {
            ix,
            shared,
            factor,
            cells: Vec::new(),
            row: 0,
            path: Vec::new(),
        }
    }

    fn block(mut self, root: &'d Step, title: Option<String>, note: Option<String>) -> Block {
        let top = self.visit(root);
        self.cells[top].c1 = self.cells[top].c0;
        Block {
            title,
            note,
            cols: self.cells[top].c0 + 1,
            rows: self.row,
            cells: self.cells,
        }
    }

    /// The step an input expands into: a step whose result has one consumer.
    fn subtree(&self, input: &Input) -> Option<&'d Step> {
        let id = input.r#ref.as_str();
        self.ix.step(id).filter(|_| {
            !self.shared.contains(id) && !self.path.contains(&id) && self.path.len() < DEPTH_MAX
        })
    }

    /// Where the earliest step under `step` sits in the recipe.
    fn first_pos(&mut self, step: &'d Step) -> usize {
        let mut best = self.ix.step_pos(&step.id).unwrap_or(usize::MAX);
        self.path.push(&step.id);
        for input in &step.inputs {
            if let Some(child) = self.subtree(input) {
                best = best.min(self.first_pos(child));
            }
        }
        self.path.pop();
        best
    }

    fn push(&mut self, cell: Cell) -> usize {
        self.cells.push(cell);
        self.cells.len() - 1
    }

    fn visit(&mut self, step: &'d Step) -> usize {
        if step.inputs.is_empty() {
            let cell = Cell {
                key: step.id.clone(),
                label: chart_label(step),
                kind: CellKind::Setup,
                r0: self.row,
                r1: self.row,
                c0: 0,
                c1: 0,
                optional: step.optional,
                step: step.id.clone(),
            };
            self.row += 1;
            return self.push(cell);
        }
        self.path.push(&step.id);
        let mut subtrees: Vec<(usize, &'d Step)> = Vec::new();
        let mut leaves: Vec<&'d Input> = Vec::new();
        for input in &step.inputs {
            match self.subtree(input) {
                Some(child) => subtrees.push((self.first_pos(child), child)),
                None => leaves.push(input),
            }
        }
        subtrees.sort_by_key(|(first, _)| *first);
        let mut children: Vec<usize> = Vec::new();
        for (_, child) in subtrees {
            children.push(self.visit(child));
        }
        for input in leaves {
            let id = input.r#ref.as_str();
            let kind = self.ix.kind(id);
            let mut label = use_label(self.ix, input, self.factor, false);
            match kind {
                Some(Kind::Ingredient) => {}
                Some(Kind::Byproduct) => {
                    label.push_str(" ← from ");
                    label.push_str(self.ix.producer(id).unwrap_or_default());
                }
                _ => label.push_str(" ←"),
            }
            let leaf = kind == Some(Kind::Ingredient);
            let cell = Cell {
                key: input.key(&step.id),
                label,
                kind: if leaf { CellKind::Leaf } else { CellKind::Ref },
                r0: self.row,
                r1: self.row,
                c0: 0,
                c1: 0,
                optional: self.ix.ingredient(id).is_some_and(|i| i.optional),
                step: step.id.clone(),
            };
            self.row += 1;
            children.push(self.push(cell));
        }
        self.path.pop();
        let column = 1 + children
            .iter()
            .map(|child| self.cells[*child].c0)
            .max()
            .unwrap_or(0);
        for child in &children {
            self.cells[*child].c1 = column - 1;
        }
        let rows = children.iter().map(|child| &self.cells[*child]);
        let cell = Cell {
            key: step.id.clone(),
            label: chart_label(step),
            kind: CellKind::Step,
            r0: rows.clone().map(|cell| cell.r0).min().unwrap_or(self.row),
            r1: rows.map(|cell| cell.r1).max().unwrap_or(self.row),
            c0: column,
            c1: column,
            optional: step.optional,
            step: step.id.clone(),
        };
        self.push(cell)
    }
}

/// Which steps are done and which are ready to start, keyed by step id.
pub fn cook_marks<S: AsRef<str>>(doc: &RecipeDoc, done: &[S]) -> Marks {
    let done: HashSet<&str> = done.iter().map(AsRef::as_ref).collect();
    let ix = Index::build(doc);
    let mut marks = Marks::new();
    for (_, step) in ix.steps() {
        let id = step.id.as_str();
        if done.contains(id) {
            marks.insert(id.to_owned(), Mark::Done);
        } else if crate::cook::is_ready(&ix, step, &done) {
            marks.insert(id.to_owned(), Mark::Ready);
        }
    }
    marks
}

const LEAF_MAX: usize = 28;
const STEP_MAX: usize = 9;

fn drawable(block: &Block, cell: &Cell) -> bool {
    cell.r0 <= cell.r1 && cell.r1 < block.rows && cell.c0 <= cell.c1 && cell.c1 < block.cols
}

fn marked_text(cell: &Cell, marks: &Marks) -> String {
    let step_done = marks.get(&cell.step) == Some(&Mark::Done);
    let key = if cell.kind != CellKind::Step && step_done {
        &cell.step
    } else {
        &cell.key
    };
    let input_row = matches!(cell.kind, CellKind::Leaf | CellKind::Ref);
    let mark = marks
        .get(key)
        .filter(|mark| !input_row || **mark == Mark::Done)
        .map_or("", |mark| mark.glyph());
    format!("{mark} {}", cell.label).trim().to_owned()
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

fn glyph(bits: u8) -> Option<char> {
    Some(match bits {
        0 => return None,
        b if b == UP | DOWN | LEFT | RIGHT => '┼',
        b if b == UP | DOWN | RIGHT => '├',
        b if b == UP | DOWN | LEFT => '┤',
        b if b == LEFT | RIGHT | DOWN => '┬',
        b if b == LEFT | RIGHT | UP => '┴',
        b if b == DOWN | RIGHT => '┌',
        b if b == DOWN | LEFT => '┐',
        b if b == UP | RIGHT => '└',
        b if b == UP | LEFT => '┘',
        b if b & (UP | DOWN) == 0 => '─',
        _ => '│',
    })
}

/// The block drawn with box characters, with cook-mode marks in front of the
/// labels. Cells outside the block's rectangle are not drawn.
pub fn chart_ascii(block: &Block, marks: &Marks) -> String {
    let cells: Vec<&Cell> = block.cells.iter().filter(|c| drawable(block, c)).collect();
    let texts: Vec<String> = cells.iter().map(|cell| marked_text(cell, marks)).collect();
    let needs: Vec<usize> = cells
        .iter()
        .zip(&texts)
        .map(|(cell, text)| {
            let cap = if cell.c0 == 0 { LEAF_MAX } else { STEP_MAX };
            let longest = text.split_whitespace().map(py::len).max().unwrap_or(0);
            longest.max(cap.min(py::len(text)))
        })
        .collect();

    let mut width = vec![3usize; block.cols];
    for (cell, need) in cells.iter().zip(&needs) {
        if cell.c0 == cell.c1 {
            width[cell.c0] = width[cell.c0].max(*need);
        }
    }
    let span = |width: &[usize], cell: &Cell| {
        width[cell.c0..=cell.c1].iter().sum::<usize>() + 3 * (cell.c1 - cell.c0)
    };
    for (cell, need) in cells.iter().zip(&needs) {
        if cell.c0 != cell.c1 {
            let available = span(&width, cell);
            if available < *need {
                width[cell.c0] += need - available;
            }
        }
    }
    let lines: Vec<Vec<String>> = cells
        .iter()
        .zip(&texts)
        .map(|(cell, text)| {
            let wrapped = py::wrap(text, span(&width, cell));
            if wrapped.is_empty() {
                vec![String::new()]
            } else {
                wrapped
            }
        })
        .collect();

    let mut height = vec![1usize; block.rows];
    for (cell, lines) in cells.iter().zip(&lines) {
        if cell.r0 == cell.r1 {
            height[cell.r0] = height[cell.r0].max(lines.len());
        }
    }
    let mut by_span: Vec<usize> = (0..cells.len()).collect();
    by_span.sort_by_key(|n| cells[*n].r1 - cells[*n].r0);
    for n in by_span {
        let cell = cells[n];
        let available = height[cell.r0..=cell.r1].iter().sum::<usize>() + (cell.r1 - cell.r0);
        if available < lines[n].len() {
            height[cell.r1] += lines[n].len() - available;
        }
    }

    let mut x_edges = vec![0usize];
    for w in &width {
        x_edges.push(x_edges[x_edges.len() - 1] + w + 3);
    }
    let mut y_edges = vec![0usize];
    for h in &height {
        y_edges.push(y_edges[y_edges.len() - 1] + h + 1);
    }
    let columns = x_edges[x_edges.len() - 1] + 1;
    let rows = y_edges[y_edges.len() - 1] + 1;
    let mut bits = vec![vec![0u8; columns]; rows];
    let mut canvas = vec![vec![' '; columns]; rows];
    for (cell, lines) in cells.iter().zip(&lines) {
        let (x0, x1) = (x_edges[cell.c0], x_edges[cell.c1 + 1]);
        let (y0, y1) = (y_edges[cell.r0], y_edges[cell.r1 + 1]);
        for x in x0..x1 {
            for y in [y0, y1] {
                bits[y][x] |= RIGHT;
                bits[y][x + 1] |= LEFT;
            }
        }
        for y in y0..y1 {
            for x in [x0, x1] {
                bits[y][x] |= DOWN;
                bits[y + 1][x] |= UP;
            }
        }
        let inner_height = y1 - y0 - 1;
        let inner_width = (x1 - x0).saturating_sub(3);
        let top = y0 + 1 + inner_height.saturating_sub(lines.len()) / 2;
        for (n, line) in lines.iter().enumerate() {
            let pad = if cell.kind == CellKind::Step {
                inner_width.saturating_sub(py::len(line)) / 2
            } else {
                0
            };
            for (k, c) in line.chars().enumerate() {
                let slot = canvas
                    .get_mut(top + n)
                    .and_then(|row| row.get_mut(x0 + 2 + pad + k));
                if let Some(slot) = slot {
                    *slot = c;
                }
            }
        }
    }
    for (canvas_row, bits_row) in canvas.iter_mut().zip(&bits) {
        for (slot, bits) in canvas_row.iter_mut().zip(bits_row) {
            if let Some(line) = glyph(*bits) {
                *slot = line;
            }
        }
    }
    let body = canvas
        .iter()
        .map(|row| row.iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    match &block.title {
        Some(title) if !title.is_empty() => {
            let note = match &block.note {
                Some(note) if !note.is_empty() => format!("  ({note})"),
                _ => String::new(),
            };
            format!("{}{note}\n{body}", title.to_uppercase())
        }
        _ => body,
    }
}

/// The block as an HTML table with `rowspan` and `colspan` taken from the
/// cell rectangles, and the cell key in `data-key`.
pub fn chart_html(block: &Block) -> String {
    let mut out = Vec::new();
    if let Some(title) = block.title.as_deref().filter(|title| !title.is_empty()) {
        let note = match block.note.as_deref().filter(|note| !note.is_empty()) {
            Some(note) => format!(" <small>{}</small>", py::html_escape(note)),
            None => String::new(),
        };
        out.push(format!("<h4>{}{note}</h4>", py::html_escape(title)));
    }
    out.push("<table class=\"chart\">".to_owned());
    for row in 0..block.rows {
        let mut cells: Vec<&Cell> = block.cells.iter().filter(|c| c.r0 == row).collect();
        cells.sort_by_key(|cell| cell.c0);
        let mut line = "  <tr>".to_owned();
        for cell in cells {
            let optional = if cell.optional { " optional" } else { "" };
            let mut attributes = format!(
                "class=\"{}{optional}\" data-key=\"{}\"",
                cell.kind.as_str(),
                py::html_escape(&cell.key)
            );
            if cell.r1 > cell.r0 {
                attributes.push_str(&format!(" rowspan=\"{}\"", cell.r1 - cell.r0 + 1));
            }
            if cell.c1 > cell.c0 {
                attributes.push_str(&format!(" colspan=\"{}\"", cell.c1 - cell.c0 + 1));
            }
            line.push_str(&format!(
                "<td {attributes}>{}</td>",
                py::html_escape(&cell.label)
            ));
        }
        line.push_str("</tr>");
        out.push(line);
    }
    out.push("</table>".to_owned());
    out.join("\n")
}
