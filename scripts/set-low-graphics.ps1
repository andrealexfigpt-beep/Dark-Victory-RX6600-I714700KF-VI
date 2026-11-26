# Apply the Low 720p profile to a session copy of settings.xml.
param([Parameter(Mandatory)][string]$SettingsFile)

$ErrorActionPreference = 'Stop'
$SettingsFile = (Resolve-Path -LiteralPath $SettingsFile).Path
$document = [System.Xml.XmlDocument]::new()
$document.PreserveWhitespace = $true
$document.Load($SettingsFile)
$lowSettings = @{
    'video/uiResolution/width' = '1280'
    'video/uiResolution/height' = '720'
    'video/resolutionFullscreen/width' = '1280'
    'video/resolutionFullscreen/height' = '720'
    'advancedGraphics/damageModelsEnabled' = 'false'
    'advancedGraphics/grassLod' = '0.350000'
    'advancedGraphics/lodScale' = '0.650000'
    'advancedGraphics/lensFlaresScreenResDivExponent' = '3'
    'advancedGraphics/manualMipBias' = '0.000000'
    'advancedGraphics/motionBlur' = 'false'
    'advancedGraphics/rtDynamicShadows' = 'false'
    'advancedGraphics/shadowParticleShadows' = 'false'
    'advancedGraphics/sharpenEnabled' = 'false'
    'advancedGraphics/ssroSampleCount' = '1'
    'advancedGraphics/waterReflectionResX' = '256'
    'advancedGraphics/waterReflectionResY' = '128'
    'advancedGraphics/waterReflectionSSR' = 'false'
}
foreach ($setting in $lowSettings.GetEnumerator()) {
    $element = $document.DocumentElement.SelectSingleNode($setting.Key)
    if ($null -eq $element -or -not $element.HasAttribute('value')) {
        throw "Missing graphics setting $($setting.Key) in $SettingsFile."
    }
    $element.SetAttribute('value', $setting.Value)
}
$document.Save($SettingsFile)
