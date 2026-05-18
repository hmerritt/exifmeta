# Exif Metadata

A simple program to read a standardised `metadata.yaml` file and add the information as exif to all image files in the same directory.

## `metadata.yaml` file

See example files in [`./examples`](./examples/metadata.yml) directory.

| Parent Interface                 | Property                 |
| :------------------------------- | :----------------------- |
| `roll`                           | `number`                 |
| `date`                           | `string` -> `YYYY-MM-DD` |
| `date_end`                       | `string` -> `YYYY-MM-DD` |
| `frame_count`                    | `number`                 |
| `notable_frames`                 | `number[]`               |
| `locations`                      | `string[]`               |
| `comment`                        | `string`                 |
| `by`                             | `string[]`               |
| `camera -> make`                 | `string`                 |
| `camera -> model`                | `string`                 |
| `lenses -> lens -> make`         | `string`                 |
| `lenses -> lens -> model`        | `string`                 |
| `lenses -> lens -> focal_length` | `string`                 |
| `film -> format`                 | `number`                 |
| `film -> stock`                  | `stock`                  |
| `film -> stock -> manufacturer`  | `string`                 |
| `film -> stock -> name`          | `string`                 |
| `film -> stock -> iso`           | `number`                 |
| `film -> stock -> color`         | `boolean`                |
| `film -> stock -> negative`      | `boolean`                |
| `development -> developer`       | `string`                 |
| `development -> process`         | `string`                 |
| `development -> developing_date` | `string`                 |
| `development -> developed_by`    | `string`                 |
| `scans -> scan -> lab`           | `boolean`                |
| `scans -> scan -> frames`        | `number[]`               |
| `scans -> scan -> scanner`       | `string`                 |
| `scans -> scan -> resolution`    | `string`                 |
