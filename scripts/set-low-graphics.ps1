# Apply the Low Full HD profile to a session copy of settings.xml.
param([Parameter(Mandatory)][string]$SettingsFile)

$ErrorActionPreference = 'Stop'
$SettingsFile = (Resolve-Path -LiteralPath $SettingsFile).Path
$document = [System.Xml.XmlDocument]::new()
$document.PreserveWhitespace = $true
$document.Load($SettingsFile)
$lowSettings = @{
    'video/uiResolution/width' = '1920'
    'video/uiResolution/height' = '1080'
    'video/resolutionFullscreen/width' = '1920'
    'video/resolutionFullscreen/height' = '1080'
    'advancedGraphics/damageModelsEnabled' = 'false'
    'advancedGraphics/lensFlaresScreenResDivExponent' = '3'
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
