# Fond chart: lines and the pinned column

How the chart in the Fond app (`crates/omni-recipes-web/src/ui/chart.rs`,
"The chart" in `style/app.css`) draws its lines and what stays in place while
it scrolls. Every line and every missing line below is meant; the browser
tests in `e2e/chart-borders.spec.js` check them in every recipe.

## What the chart says

One row per input (an ingredient, an earlier component, a by-product). A step
is a cell to the right of everything it combines, as tall as those rows
together. Reading left to right is reading forward in time; the last column
is the finished dish. A multi-component recipe is several such tables, each
under its own title.

## Lines

| Line | Meaning | Drawn as |
| --- | --- | --- |
| Frame around a block | one component, start to finish | strong, all four sides |
| Left side of a step cell | "these rows go into this step" | strong |
| Under a step cell, under the last input of a step | the step ends here; what is below belongs to another step | strong |
| Between two inputs of the same step | two things, one step | light |
| Under or over a set-up step (heat the oven) | a step with no inputs, on its own | strong |

Strong is `--line-strong`, light is `--line`. The two weights show the
grouping the layout already has: the flour, the baking soda and the salt are
three rows with light lines between them and a strong line around the three
and their "whisk".

Lines that are left out on purpose:

- **Inside an input that waits for a later step.** Eggs that go into "beat
  in" two columns to the right are one cell from the ingredient column to that
  step. It is built from two table cells (the pinned label and an empty
  stretch), and there is no line between the two: a line there would read as
  a step that does nothing.
- **Inside a step cell.** A step spanning eight rows has no rules across it;
  the rows end at its left side.
- **No double lines.** Each line belongs to exactly one cell: every cell
  draws its right and its bottom, the first row adds the frame's top and the
  ingredient column the frame's left. Nothing is drawn twice, so no line is
  ever two pixels thick.

In cook mode a ready step has an accent ring. A ready set-up step that
stretches right gets one ring around the whole cell, not one around each of
its two halves.

## What stays in place

- **The ingredient column is pinned** while a wide chart scrolls sideways.
  Every row starts there, so it is the one column that tells you what a step
  cell on screen is made of.
- **Its edge appears only while scrolled.** At rest the column is just the
  first column of the table. Scrolled, it is a pane over the steps passing
  under it, so it gets a full-height line and a short shadow on its right
  (`.chart-edge`), including on the rows that have no line there at rest. The
  line lies over the last pixel of the column, exactly where the other rows
  already have theirs, so it never doubles one.
- **Block titles do not scroll sideways.** They sit outside the scrolling
  area.
- **Up and down is the screen's job.** The chart sits inside the screen's
  one vertical scroller ("The shell" in `docs/recipes-app.md`); a block's own
  scroller moves only sideways and keeps its own bounce at both ends.
- **Nothing is pinned to the top.** The chart has no header row: its columns
  are steps in order, not named fields. Titles are not pinned to the top of
  the page either. A block is seldom much taller than the screen, and cook
  mode already keeps two bars there.
- **Step columns are not pinned.** Which one would matter changes as you
  cook.

A wide chart runs off the right side of the screen on purpose, as the sign
that it scrolls. Scrolled to its end, the frame's right line is on screen
with the page margin after it.

## WebKit pitfalls and how the CSS avoids them

- **`border-collapse: collapse` with sticky cells.** Collapsed borders belong
  to the table grid, not to a cell, and WebKit leaves them behind when a
  sticky cell moves: the pinned column loses its lines mid-scroll. The table
  is `border-collapse: separate` with `border-spacing: 0`, and the one-owner
  rule above replaces collapsing.
- **A pinned cell without a right line.** The label of a stretching input
  has no right border (see above). Scrolled, the next step cell used to butt
  straight against the label, which looked like a missing border. The edge
  element supplies the line whenever the block is scrolled.
- **Fractions of a pixel.** `logic::chart_widths` can return fractional
  widths, and a 14px font at line height 1.25 makes 17.5px lines, so rows
  were 32.5px high. On a 3x screen such edges fall between device pixels and
  neighbouring lines come out with different thicknesses. Columns are rounded
  to whole pixels (`whole_pixels` in `chart.rs`; the ingredient column takes
  the remainder) and the line height is 18px, so every column and row is a
  whole number of pixels.
- **See-through pinned cells.** A sticky cell needs an opaque background of
  its own or the steps show through it; every state of a cell (plain, done,
  ready) sets one.
- **Shadows on table cells.** A `box-shadow` per pinned cell breaks at every
  row and fights the ready ring, which is also a `box-shadow`. The shadow is
  one element beside the table instead, positioned from the ingredient
  column's width.
- **Padding at the scroll end.** The scrolling area has no left padding, so
  the pinned column sits on its edge and nothing slides past beside it; the
  right padding is the page margin after the frame's last line.
