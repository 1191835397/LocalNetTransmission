param(
    [Parameter(Mandatory = $true)][int]$ApplicationPid,
    [Parameter(Mandatory = $true)][string]$FilePath,
    [switch]$Directory
)
$ErrorActionPreference = 'Stop'
$filenameId = if ($Directory) { '1152' } else { '1148' }
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class NativeDialogControls {
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll")]
    public static extern bool IsChild(IntPtr parent, IntPtr child);
    [DllImport("user32.dll", EntryPoint = "SendMessageTimeoutW", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SetText(IntPtr window, uint message, IntPtr parameter, string text, uint flags, uint timeout, out IntPtr result);
    [DllImport("user32.dll", EntryPoint = "PostMessageW", SetLastError = true)]
    public static extern bool PostMessage(IntPtr window, uint message, IntPtr parameter, IntPtr data);
}
'@
$condition = New-Object System.Windows.Automation.AndCondition(
    (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ApplicationPid)),
    (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, '#32770'))
)
$deadline = [DateTime]::UtcNow.AddSeconds(10)
$dialog = $null
$filename = $null
$openButton = $null
while ([DateTime]::UtcNow -lt $deadline) {
    $dialog = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
    if ($null -ne $dialog) {
        $filename = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
            (New-Object System.Windows.Automation.AndCondition(
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Edit')),
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::AutomationIdProperty, $filenameId)))))
        $openButton = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
            (New-Object System.Windows.Automation.AndCondition(
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'Button')),
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::AutomationIdProperty, '1')))))
        if ($null -ne $filename -and $null -ne $openButton) { break }
    }
    Start-Sleep -Milliseconds 100
}
if ($null -eq $dialog -or $null -eq $filename -or $null -eq $openButton) {
    if ($null -ne $dialog) {
        $dialog.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition) | ForEach-Object {
            [PSCustomObject]@{ Name = $_.Current.Name; Class = $_.Current.ClassName; Id = $_.Current.AutomationId }
        } | Format-Table -AutoSize | Out-String -Width 240 | Write-Output
    }
    throw 'Native file dialog controls not found for this application process.'
}
$dialogHandle = [IntPtr]$dialog.Current.NativeWindowHandle
foreach ($control in @($filename, $openButton)) {
    $handle = [IntPtr]$control.Current.NativeWindowHandle
    [uint32]$ownerId = 0
    [void][NativeDialogControls]::GetWindowThreadProcessId($handle, [ref]$ownerId)
    if ($ownerId -ne $ApplicationPid -or -not [NativeDialogControls]::IsChild($dialogHandle, $handle)) {
        throw 'Refusing to interact with a control outside the application dialog.'
    }
}
# This Windows build exposes legacy controls as Pane without ValuePattern or InvokePattern.
[IntPtr]$result = [IntPtr]::Zero
$sent = [NativeDialogControls]::SetText([IntPtr]$filename.Current.NativeWindowHandle, 0x000C, [IntPtr]::Zero, $FilePath, 2, 3000, [ref]$result)
if ($sent -eq [IntPtr]::Zero -or $result -eq [IntPtr]::Zero) { throw 'Unable to set the file name.' }
if (-not [NativeDialogControls]::PostMessage([IntPtr]$openButton.Current.NativeWindowHandle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)) {
    throw 'Unable to activate the Open button.'
}
