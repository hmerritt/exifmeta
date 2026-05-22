#!/usr/bin/env python3
"""Convert GeoNames cities1000.txt into a compact SQLite database."""

from __future__ import annotations

import argparse
import csv
import sqlite3
from contextlib import closing
from pathlib import Path


EXPECTED_FIELD_COUNT = 19
DEFAULT_INPUT = Path("cities1000.txt")
DEFAULT_OUTPUT = Path("assets/geonames/cities1000.sqlite")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Convert GeoNames cities1000.txt into compact SQLite."
    )
    parser.add_argument("--input", "-i", type=Path, default=DEFAULT_INPUT)
    parser.add_argument("--output", "-o", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()

    rows = convert(args.input, args.output)
    print(f"wrote {rows} rows to {args.output} ({args.output.stat().st_size} bytes)")


def convert(input_path: Path, output_path: Path) -> int:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    if output_path.exists():
        output_path.unlink()

    with closing(sqlite3.connect(output_path)) as connection:
        connection.execute("PRAGMA journal_mode = OFF")
        connection.execute("PRAGMA synchronous = OFF")
        create_schema(connection)
        row_count = import_rows(connection, input_path)
        connection.execute(
            """
            CREATE INDEX locations_name_country_idx
            ON locations(name COLLATE NOCASE, country_code)
            """
        )
        connection.commit()
        connection.execute("VACUUM")

    return row_count


def create_schema(connection: sqlite3.Connection) -> None:
    connection.execute(
        """
        CREATE TABLE locations (
            geoname_id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            country_code TEXT NOT NULL,
            latitude REAL NOT NULL,
            longitude REAL NOT NULL,
            population INTEGER NOT NULL,
            elevation INTEGER
        )
        """
    )


def import_rows(connection: sqlite3.Connection, input_path: Path) -> int:
    rows = 0

    with input_path.open("r", encoding="utf-8", newline="") as file:
        reader = csv.reader(file, delimiter="\t")
        records = []

        for line_number, fields in enumerate(reader, start=1):
            if len(fields) != EXPECTED_FIELD_COUNT:
                raise ValueError(
                    f"line {line_number}: expected {EXPECTED_FIELD_COUNT} fields, "
                    f"found {len(fields)}"
                )

            records.append(
                (
                    parse_int(fields[0], "geonameid", line_number),
                    fields[1],
                    fields[8],
                    parse_float(fields[4], "latitude", line_number),
                    parse_float(fields[5], "longitude", line_number),
                    parse_int(fields[14], "population", line_number),
                    parse_optional_int(fields[15], "elevation", line_number),
                )
            )

            if len(records) >= 10_000:
                insert_records(connection, records)
                rows += len(records)
                records.clear()

        if records:
            insert_records(connection, records)
            rows += len(records)

    return rows


def insert_records(connection: sqlite3.Connection, records: list[tuple]) -> None:
    connection.executemany(
        """
        INSERT INTO locations (
            geoname_id,
            name,
            country_code,
            latitude,
            longitude,
            population,
            elevation
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        """,
        records,
    )


def parse_int(value: str, field: str, line_number: int) -> int:
    try:
        return int(value)
    except ValueError as error:
        raise ValueError(f"line {line_number}: invalid {field}: {error}") from error


def parse_optional_int(value: str, field: str, line_number: int) -> int | None:
    if value == "":
        return None

    return parse_int(value, field, line_number)


def parse_float(value: str, field: str, line_number: int) -> float:
    try:
        return float(value)
    except ValueError as error:
        raise ValueError(f"line {line_number}: invalid {field}: {error}") from error


if __name__ == "__main__":
    main()
