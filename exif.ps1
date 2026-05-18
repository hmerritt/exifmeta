#
# Edit EXIF v1.5:
# - Reads `metadata.yml` for film roll data
# - Edits all JPG, JPEG, PNG, and TIF files in the current directory to add ex
#
# Requires: exiftool — `scoop install exiftool`
#

$exiftoolPath = "exiftool"
$metadataFile = "metadata.yml"

if (-not (Test-Path $metadataFile)) {
    Write-Error "The file $metadataFile was not found."
    exit
}

$yml = Get-Content $metadataFile -Raw

function Get-YmlValue($key) {
    if ($yml -match "(?m)^\s*${key}:\s*(.*)") {
        return $Matches[1].Trim().Trim('"').Trim("'")
    }
    return $null
}

function Get-YmlFirstListValue($section, $key) {
    if ($yml -match "(?ms)^\s*${section}:\s*\r?\n(?<body>\s*-\s.*?)(?=^\S|\z)") {
        $body = $Matches["body"]
        if ($body -match "(?m)^\s*(?:-\s*)?${key}:\s*(.*)") {
            return $Matches[1].Trim().Trim('"').Trim("'")
        }
    }
    return $null
}

function Get-YmlListValues($section, $key) {
    if ($yml -notmatch "(?ms)^\s*${section}:\s*\r?\n(?<body>.*?)(?=^\S|\z)") {
        return @()
    }

    $body = $Matches["body"]
    $values = [regex]::Matches($body, "(?m)^\s*(?:-\s*)?${key}:\s*(.*)") | ForEach-Object {
        $_.Groups[1].Value.Trim().Trim('"').Trim("'")
    }

    return @($values | Where-Object { $_ })
}

function Get-YmlScalarListValues($section) {
    if ($yml -notmatch "(?ms)^\s*${section}:\s*\r?\n(?<body>.*?)(?=^\S|\z)") {
        return @()
    }

    $body = $Matches["body"]
    $values = [regex]::Matches($body, "(?m)^\s*-\s*(.+?)\s*$") | ForEach-Object {
        $_.Groups[1].Value.Trim().Trim('"').Trim("'")
    }

    return @($values | Where-Object { $_ })
}

# Extraction of Base Data
$make = Get-YmlValue "make"
$model = Get-YmlValue "model"
$fmt = Get-YmlValue "format"
$mfg = Get-YmlValue "manufacturer"
$name = Get-YmlValue "name"
$iso = Get-YmlValue "iso"
$lensMake = Get-YmlFirstListValue "lenses" "make"
$lensModel = Get-YmlFirstListValue "lenses" "model"
$focalLength = Get-YmlFirstListValue "lenses" "focal_length"
$authors = Get-YmlScalarListValues "by"
$authorText = $authors -join ", "
$dateString = Get-YmlValue "date" # "2025-08-29"
$stock = "$mfg $name"

# Initialize Base Time at 12:00:00 PM
$baseDate = [DateTime]::ParseExact($dateString, "yyyy-MM-dd", $null).AddHours(12)

# Ensure images are sorted alphabetically/numerically
$imageExtensions = @(".jpg", ".jpeg", ".png", ".tif")
$images = Get-ChildItem -File | Where-Object { $_.Extension -in $imageExtensions } | Sort-Object Name

if ($images.Count -eq 0) {
    Write-Host "No JPG, JPEG, PNG, or TIF files found." -ForegroundColor Yellow
    exit
}

Write-Host "Processing $($images.Count) images with 10-second increments..." -ForegroundColor Cyan

$incrementSeconds = 0

foreach ($img in $images) {
    # Calculate the incremental time for the current frame
    $currentTime = $baseDate.AddSeconds($incrementSeconds)

    # Format for ExifTool: "YYYY:MM:DD HH:MM:SS"
    $formattedDate = $currentTime.ToString("yyyy:MM:dd HH:mm:ss")

    $arguments = @(
        "-overwrite_original",
        "-Make=$make",
        "-Model=$model",
        "-LensMake=$lensMake",
        "-LensModel=$lensModel",
        "-FocalLength=$focalLength",
        "-ISO=$iso",
        "-XMP-dc:Subject=$fmt",
        "-UserComment=Film: $stock, Format: $fmt",
        "-Description=Film Stock: $stock",
        "-DateTimeOriginal=$formattedDate",
        "-CreateDate=$formattedDate",
        "-ModifyDate=$formattedDate",
        "-FileCreateDate=$formattedDate",
        "-FileModifyDate=$formattedDate",
        $img.FullName
    )

    if ($authors.Count -gt 0) {
        $arguments = $arguments[0..($arguments.Count - 2)] + "-XMP-dc:Creator=" + $arguments[-1]
        foreach ($author in $authors) {
            $arguments = $arguments[0..($arguments.Count - 2)] + "-XMP-dc:Creator+=$author" + $arguments[-1]
        }
    }

    if ($authorText) {
        $arguments = $arguments[0..($arguments.Count - 2)] + @("-Artist=$authorText", "-XPAuthor=$authorText") + $arguments[-1]
    }

    & $exiftoolPath $arguments

    # Advance the clock by 60 seconds for the next file
    $incrementSeconds += 60
}

Write-Host "Batch processing complete. Frames are now chronologically ordered." -ForegroundColor Green
