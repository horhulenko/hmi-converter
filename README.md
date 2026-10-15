# HMI Log Converter

Turns WinCC HMI logging databases into one flat CSV table.

```
Tags.db3     LoggingTag           tag numbers and their names
Values.db3   LoggedProcessValue   the logged values
```

An HMI starts a new value database every so often, so a week of history is a
row of files rather than one. Drop **as many as you like, in any order, with
any file name** — or the whole folder, which is searched for databases. Every
file is opened to see what it holds, so the names do not matter. The source
databases are only ever read, never changed.

## Build

```bash
cargo build --release
```

The program is `target/release/hmi-converter.exe`. It needs no installation and
no runtime.

The release profile is already built for size — optimised for space rather than
speed, link-time optimisation across crates, one code unit, no unwind tables and
no symbols. To go further, pack the finished file:

```bash
upx --best --lzma target/release/hmi-converter.exe
```

11.4 MB becomes 4.3 MB. It is the same program: it unpacks itself into memory
when it starts, which costs a fraction of a second once. Some antivirus
scanners look twice at packed executables, so ship the plain one where that
matters.

## Using the window

1. Drag the `.db3` files onto the window, or their whole folder (or press
   **Choose files…** / **Choose a folder…**).
2. Look over **Files to import** and untick anything you do not want.
3. Optionally take a slice out of the log under **Time range**.
4. Check the name under **Save to** and what to write under **What to write**.
5. Press **Convert to CSV**.

### Files to import

One row per database found, in the order they are read:

| column | |
| --- | --- |
| tick | whether the file takes part in this import |
| Holds | `tag names`, `values`, or both |
| File | its name; click it to show the file in Explorer |
| Values | how many of its values the import will write |
| Logged from | when it starts, and how long it covers |
| Size | the file on disk |

Including and excluding is meant to be quick:

* a **tick**, or a click anywhere on the row
* **shift**-click to carry that on to every row back to the last one ticked
* **right-click** for that file and no other
* **All**, **None**, **Invert**, and a filter box once the list is long
* the **✕** at the end of a row takes the file off the list altogether

Underneath, the totals say what the import adds up to. They also warn when no
tag database is in (tags are then written as `id_<number>`) and when two files
cover the same period, which would write those readings twice.

### Time range

The files are drawn on one axis, each as a bar where it logs — **bright where
it really logged, faint where it did not**, so a night the plant stood still is
visible before converting rather than after.

* **drag across the bar** to take a slice
* **drag its edges** to adjust it, or the middle to move it; the edges snap to
  the ends of the files
* **Split** cuts a slice in two so the halves can be pulled apart, which leaves
  the quiet hour between them out of the export. Any number of slices is
  allowed, and **Whole log** forgets them again.
* the **From** and **Until** fields say exactly where the chosen slice falls,
  and can be typed into instead

While you drag, the count of what survives follows along, file by file.

The last range you converted is remembered: it appears as the grey placeholder
in the fields, and the **Use last** button fills it in again.

## Output

Two files, named after the box under **Save to**:

**`<name>_long.csv`** — one row per logged value, which is the shape most tools
want for filtering and grouping:

| column | meaning |
| --- | --- |
| `pk_TimeStamp` | the time as `18.09.2026 06:03:48.7883740` (UTC) |
| `pk_TimeStamp_Raw` | the original number from the database |
| `pk_fk_Id` | tag number, `LoggedProcessValue.pk_fk_Id` |
| `Value` | the logged value |
| `Name` | tag name, `LoggingTag.Name` |
| `Quality` | `192` means good; anything else is doubtful |
| `SourceFile` | which database the row came from — off unless you tick it |

**`<name>_wide.csv`** - one row per timestamp, one column per tag, for plotting
and for Excel. An empty cell means that tag logged nothing at that instant.

### Choosing the long table's columns

The **Columns...** button beside *Long table* opens a chooser where you can

* untick any column you do not want,
* drag a row by its handle into any order (the others slide out of the way),
* type your own heading for any column - leave it empty to keep the original.

The first line of the file is shown as you edit, and a warning appears if two
columns would end up with the same heading. Whatever you set is kept: reopening
the chooser, or the program itself, brings it all back.

### What is included

All three value tables are merged: `LoggedProcessValue` (numbers),
`LoggedProcessValueText` (text) and `LoggedProcessValue64` (large integers).
Rows are joined on `LoggedProcessValue.pk_fk_Id = LoggingTag.pk_Key`; a tag with
no name is written as `id_<number>`.

So are the files. Each database is read on its own thread and the rows are
merged by time as they arrive, so one export of ten files is one table in one
time order — not ten tables after one another. The finished screen says how
many values came out of each file, and the `SourceFile` column can put that
into the table itself.

Nothing is silently dropped. Rows of doubtful quality are kept and marked in the
`Quality` column, and the summary screen says how many there were.

### Dates

Every date, in the window and in the files, is written `dd.mm.yyyy`, and all
times are **UTC**.

The database itself stores Windows FILETIME — 100-nanosecond ticks since
1601-01-01. Both the readable time and the original number are written, so
nothing is lost.

## Time range, in writing

Anything outside the slices is ignored. Both ends of a slice are inclusive and
either may be left empty, which means "no limit at this end". Accepted
spellings:

```
18.09.2026                 a whole day
2026-09-18                 the same day
18.09.2026 06:30           to the minute
18.09.2026 06:30:45,123    to the millisecond
134341850287883740         a raw number from pk_TimeStamp_Raw
```

A bare date given as **Until** means the end of that day (23:59:59.9999999).

## Shorten tag names

Off by default. When ticked, every name is cut down to the part that actually
tells the tags apart:

```
Anlage1::Zustand_DruckvorPumpe2:Archivvariable_1      ->  DruckvorPumpe2
Anlage1::Zustand_IstwertVentilPumpe1:Archivvar…  ->  IstwertVentilPumpe1
```

Whatever every name has in common is removed, and so is the leading category
(`Zustand_`, `Parameter_`, …). Because the names become column headings they
must stay unique: any name that would clash with another keeps its category, and
failing that its full text. Shortening therefore never merges two tags. In the
sample export 88 of 104 names shorten; the remaining 16 keep more of their name
for exactly that reason, for example:

```
Zustand_SollwertVentilPumpe1Hand     both exist, so both keep the category
Parameter_SollwertVentilPumpe1Hand
VentilP1Endlage1_I                      would otherwise collapse to just "I"
```

The window shows a real example from your own file as soon as you tick the box.

## Charts

**Open charts** builds one HTML page beside the CSV files and hands it to your
browser. Everything is inside that page: the Apache ECharts library is compiled
into the program and copied in, so the chart works with no network, no install
and no server - and the file can be sent to someone else as it is.

Eleven ways to look at the same values, switched with one click:

| | |
| --- | --- |
| Lines, Area, Stacked, Steps, Scatter, Bars | the value against time |
| Small multiples | one little chart per tag, to skim a hundred tags at once |
| Heatmap | time across, tags down, colour for the value |
| Histogram | how often each value occurs |
| Box plot | median, quartiles and reach, tag beside tag |
| Correlation | one tag against another, coloured by time |

Hovering anywhere on a time chart lists **every chosen tag with its name and its
value at that instant**, largest first. Tags do not log in step with each other,
so a reading that still stands is marked `held` rather than invented.

The list on the left switches tags in and out as you watch, with a filter box
and All / None / Invert. **Log scale** covers values spanning orders of
magnitude; **Normalise** puts everything on 0-100% when the units differ.
Drag on the chart to zoom, or use the slider underneath.

Only the eight busiest tags are on at the start, so the first view is readable.

### Detail

The whole log is more points than a browser enjoys, so **Chart detail** decides
how many of them go into the page:

| | |
| --- | --- |
| Light | up to 1000 points per tag |
| Balanced (default) | up to 4000 points per tag |
| Full | every point, largest file |

Thinning uses *largest-triangle-three-buckets*, which keeps the shape of the
line and its peaks rather than averaging them away. The sample export at
Balanced is 83 000 of 602 922 points and makes a 3.4 MB page.

Tags that log text rather than numbers cannot go on an axis; they are left out
and counted on the finished screen.

## Command line

Passing file or folder names converts without opening a window:

```bash
hmi-converter Tags.db3 Values.db3 -o output_folder
hmi-converter D:\HMI\export -o out            # the whole folder, however many files
hmi-converter data/*.db3 --from "18.09.2026 07:00" --until "18.09.2026 07:30"
```

It prints what it found and what each file contributed. The window is the place
for several slices of time; the command line takes the one `--from`/`--until`.

| option | meaning |
| --- | --- |
| `-o`, `--out FOLDER` | where to write the CSV files |
| `-n`, `--name PREFIX` | name of the files, before `_long` / `_wide` |
| `--from WHEN` | ignore entries before this date/time |
| `--until WHEN` | ignore entries after this date/time |
| `--short-names` | shorten tag names to the telling part |
| `--columns LIST` | long-table columns, in order: `time,raw,id,value,name,quality,file`; add `=HEADING` to rename one, e.g. `--columns "time=Zeit,name=Messstelle,value=Wert"` |
| `--semicolon` | use `;` instead of `,` (Excel with a comma decimal mark) |
| `--long-only`, `--wide-only` | write just one of the two tables |
| `--chart` | also write the interactive chart page |
| `--detail LEVEL` | chart detail: `full`, `balanced` or `light` |
| `--gui` | open the window with these files already loaded |

`--gui` is what makes dropping databases, or their folder, onto the `.exe`
useful: put a shortcut with that flag wherever it is handy.

## Tests

```bash
cargo test
```

covers the FILETIME conversion in both directions, the date parser, the chart
clock and its point-thinning, the column names, the SQL that several time
slices turn into, the tag-name shortening (including the cases that must not
merge) and CSV quoting.

## Credits

Charts are drawn by Apache ECharts (Apache-2.0), a copy of which lives in
`assets/echarts.min.js` and is compiled into the program, so the pages it
writes never need the network.
