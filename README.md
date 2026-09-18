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
   Dropping the whole folder works too.
2. Optionally narrow the **time range**. While you type it shows how many values
   would be left.
3. Press **Convert to CSV**.

The last time range you converted is remembered: it appears as the grey
placeholder in the field, and the **Use last** button fills it in again.

## Output

Two files are written next to the value database:

**`<name>_long.csv`** — one row per logged value, which is the shape most tools
want for filtering and grouping:

| column | meaning |
| --- | --- |
| `pk_TimeStamp` | the time as `2026-09-18T06:03:48.7883740Z` (UTC) |
| `pk_TimeStamp_Raw` | the original number from the database |
| `pk_fk_Id` | tag number, `LoggedProcessValue.pk_fk_Id` |
| `Value` | the logged value |
| `Name` | tag name, `LoggingTag.Name` |
| `Quality` | `192` means good; anything else is doubtful |

**`<name>_wide.csv`** — one row per timestamp, one column per tag, for plotting
and for Excel. An empty cell means that tag logged nothing at that instant.

### What is included

All three value tables are merged: `LoggedProcessValue` (numbers),
`LoggedProcessValueText` (text) and `LoggedProcessValue64` (large integers).
Rows are joined on `LoggedProcessValue.pk_fk_Id = LoggingTag.pk_Key`; a tag with
no name is written as `id_<number>`.

Nothing is silently dropped. Rows of doubtful quality are kept and marked in the
`Quality` column, and the summary screen says how many there were.

### About the timestamps

The database stores Windows FILETIME — 100-nanosecond ticks since
1601-01-01 UTC. Both the readable UTC time and the original number are written,
so nothing is lost. **All times, including the range you type, are UTC.**

## Time range

Anything before **From** or after **Until** is ignored. Both ends are inclusive
and either may be left empty. Accepted spellings:

```
2026-09-18                 a whole day
18.09.2026                 the same day
2026-09-18 06:30           to the minute
2026-09-18T06:30:45.123    to the millisecond
134341850287883740         a raw number from pk_TimeStamp_Raw
```

A bare date given as **Until** means the end of that day (23:59:59.9999999).

## Command line

Passing file names converts without opening a window:

```bash
hmi-converter Tags.db3 Values.db3 -o output_folder
hmi-converter data/*.db3 --from "2026-09-18 07:00" --until "2026-09-18 07:30"
```

| option | meaning |
| --- | --- |
| `-o`, `--out FOLDER` | where to write the CSV files |
| `--from WHEN` | ignore entries before this date/time |
| `--until WHEN` | ignore entries after this date/time |
| `--semicolon` | use `;` instead of `,` (Excel with a comma decimal mark) |
| `--long-only`, `--wide-only` | write just one of the two tables |

## Tests

```bash
cargo test
```

covers the FILETIME conversion in both directions, the date parser and CSV
quoting.
