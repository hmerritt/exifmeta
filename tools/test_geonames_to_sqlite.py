from __future__ import annotations

import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path

import geonames_to_sqlite


class GeoNamesToSqliteTests(unittest.TestCase):
    def test_convert_imports_population_and_elevation(self) -> None:
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            root = Path(directory)
            input_path = root / "cities1000.txt"
            output_path = root / "cities1000.sqlite"
            input_path.write_text(
                geonames_row("123", "Testville", "4567", "89")
                + geonames_row("456", "Blankton", "1234", ""),
                encoding="utf-8",
            )

            row_count = geonames_to_sqlite.convert(input_path, output_path)

            self.assertEqual(row_count, 2)
            with closing(sqlite3.connect(output_path)) as connection:
                columns = [
                    row[1]
                    for row in connection.execute("PRAGMA table_info(locations)")
                ]
                rows = connection.execute(
                    """
                    SELECT geoname_id, name, country_code, latitude, longitude, population, elevation
                    FROM locations
                    ORDER BY geoname_id
                    """
                ).fetchall()

            self.assertIn("population", columns)
            self.assertIn("elevation", columns)
            self.assertEqual(
                rows,
                [
                    (123, "Testville", "GB", 52.5, -1.25, 4567, 89),
                    (456, "Blankton", "GB", 52.5, -1.25, 1234, None),
                ],
            )


def geonames_row(
    geoname_id: str, name: str, population: str, elevation: str
) -> str:
    return (
        "\t".join(
            [
                geoname_id,
                name,
                name,
                name,
                "52.5",
                "-1.25",
                "P",
                "PPL",
                "GB",
                "",
                "ENG",
                "",
                "",
                "",
                population,
                elevation,
                "100",
                "Europe/London",
                "2026-01-02",
            ]
        )
        + "\n"
    )


if __name__ == "__main__":
    unittest.main()
