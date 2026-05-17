#
# Edit EXIF v1.3:
# - Reads `metadata.yml` for film roll data
# - Edits all `.tif` files in the current directory to add ex
#

$exiftoolPath = "exiftool"
$metadataFile = "metadata.yml"

if (-not (Test-Path $metadataFile))
{
    Write-Error "The file $metadataFile was not found."
    exit
}

$yml = Get-Content $metadataFile -Raw

function Get-YmlValue($key)
{
    if ($yml -match "(?m)^\s*${key}:\s*(.*)")
    {
        return $Matches[1].Trim().Trim('"').Trim("'")
    }
    return $null
}

# Extraction of Base Data
$make  = Get-YmlValue "make"
$model = Get-YmlValue "model"
$fmt   = Get-YmlValue "format"
$mfg   = Get-YmlValue "manufacturer"
$name  = Get-YmlValue "name"
$dateString = Get-YmlValue "date" # "2025-08-29"
$stock = "$mfg $name"

# Initialize Base Time at 12:00:00 PM
$baseDate = [DateTime]::ParseExact($dateString, "yyyy-MM-dd", $null).AddHours(12)

# Ensure images are sorted alphabetically/numerically
$images = Get-ChildItem -Filter *.tif | Sort-Object Name

if ($images.Count -eq 0)
{
    Write-Host "No .tif files found." -ForegroundColor Yellow
    exit
}

Write-Host "Processing $($images.Count) images with 10-second increments..." -ForegroundColor Cyan

$incrementSeconds = 0

foreach ($img in $images)
{
    # Calculate the incremental time for the current frame
    $currentTime = $baseDate.AddSeconds($incrementSeconds)

    # Format for ExifTool: "YYYY:MM:DD HH:MM:SS"
    $formattedDate = $currentTime.ToString("yyyy:MM:dd HH:mm:ss")

    $arguments = @(
        "-overwrite_original",
        "-Make=$make",
        "-Model=$model",
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

    & $exiftoolPath $arguments

    # Advance the clock by 60 seconds for the next file
    $incrementSeconds += 60
}

Write-Host "Batch processing complete. Frames are now chronologically ordered." -ForegroundColor Green
