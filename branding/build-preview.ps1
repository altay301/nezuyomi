# Builds preview.html from preview.template.html by inlining each referenced SVG as a data URI,
# so the preview works when opened as a single file.
$dir = $PSScriptRoot
$html = [IO.File]::ReadAllText((Join-Path $dir 'preview.template.html'))
foreach ($svg in Get-ChildItem -Path $dir -Filter '*.svg') {
    $b64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($svg.FullName))
    $html = $html.Replace("src=""$($svg.Name)""", "src=""data:image/svg+xml;base64,$b64""")
}
[IO.File]::WriteAllText((Join-Path $dir 'preview.html'), $html, (New-Object Text.UTF8Encoding $false))
