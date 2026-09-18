# HMI Log Converter

Turns a pair of WinCC HMI logging databases into one flat CSV table.

```
Tags.db3     LoggingTag           tag numbers and their names
Values.db3   LoggedProcessValue   the logged values
```

Drop both files on the window in **any order, with any file name** — the program
looks inside each file and works out which is which by its tables. The source
databases are only ever read, never changed.

## Build

```bash
cargo build --release
```

The program is `target/release/hmi-converter.exe`. It needs no installation and
no runtime.

## Using the window

1. Drag both `.db3` files onto the window (or press **Choose files…**).
   Dropping the whole folder works too. Each loaded file can be clicked to show
   it in Explorer.
2. Optionally narrow the **time range**. While you type it shows how many values
   would be left.
3. Check the name under **Save to** and what to write under **What to write**.
4. Press **Convert to CSV**.

The last time range you converted is remembered: it appears as the grey
placeholder in the field, and the **Use last** button fills it in again.

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

Nothing is silently dropped. Rows of doubtful quality are kept and marked in the
`Quality` column, and the summary screen says how many there were.

### Dates

Every date, in the window and in the files, is written `dd.mm.yyyy`, and all
times are **UTC**.

The database itself stores Windows FILETIME — 100-nanosecond ticks since
1601-01-01. Both the readable time and the original number are written, so
nothing is lost.

## Time range

Anything before **From** or after **Until** is ignored. Both ends are inclusive
and either may be left empty. Accepted spellings:

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

Passing file names converts without opening a window:

```bash
hmi-converter Tags.db3 Values.db3 -o output_folder
hmi-converter data/*.db3 --from "18.09.2026 07:00" --until "18.09.2026 07:30"
```

| option | meaning |
| --- | --- |
| `-o`, `--out FOLDER` | where to write the CSV files |
| `-n`, `--name PREFIX` | name of the files, before `_long` / `_wide` |
| `--from WHEN` | ignore entries before this date/time |
| `--until WHEN` | ignore entries after this date/time |
| `--short-names` | shorten tag names to the telling part |
| `--columns LIST` | long-table columns, in order: `time,raw,id,value,name,quality`; add `=HEADING` to rename one, e.g. `--columns "time=Zeit,name=Messstelle,value=Wert"` |
| `--semicolon` | use `;` instead of `,` (Excel with a comma decimal mark) |
| `--long-only`, `--wide-only` | write just one of the two tables |
| `--chart` | also write the interactive chart page |
| `--detail LEVEL` | chart detail: `full`, `balanced` or `light` |
| `--gui` | open the window with these files already loaded |

`--gui` is what makes dragging the two databases onto the `.exe` useful: put a
shortcut with that flag wherever it is handy.

## Tests

```bash
cargo test
```

covers the FILETIME conversion in both directions, the date parser, the chart
clock and its point-thinning, the column
names, the tag-name shortening (including the cases that must not merge) and
CSV quoting.

## Credits

Charts are drawn by Apache ECharts (Apache-2.0), a copy of which lives in
`assets/echarts.min.js` and is compiled into the program, so the pages it
writes never need the network.
