param(
    [Parameter(Mandatory = $true)][int]$ApplicationPid,
    [Parameter(Mandatory = $true)][string]$ButtonText
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ConfirmDialogControls {
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll")]
    public static extern bool IsChild(IntPtr parent, IntPtr child);
    [DllImport("user32.dll", EntryPoint = "PostMessageW", SetLastError = true)]
    public static extern bool PostMessage(IntPtr window, uint message, IntPtr parameter, IntPtr data);
}
'@
$condition = New-Object System.Windows.Automation.AndCondition(
    (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ApplicationPid)),
    (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, '#32770'))
)
$deadline = [DateTime]::UtcNow.AddSeconds(10)
while ([DateTime]::UtcNow -lt $deadline) {
    $dialogs = [System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    foreach ($dialog in $dialogs) {
        $button = $dialog.FindFirst([System.Windows.Automation.TreeScope]::Descendants,
            (New-Object System.Windows.Automation.AndCondition(
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ClassNameProperty, 'CCPushButton')),
                (New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, $ButtonText)))))
        if ($null -eq $button) { continue }
        $invokePattern = $null
        if ($button.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$invokePattern)) {
            if ($button.Current.ProcessId -ne $ApplicationPid -or $dialog.Current.ProcessId -ne $ApplicationPid) {
                throw 'Refusing to invoke a control outside the application dialog.'
            }
            $invokePattern.Invoke()
            exit 0
        }
        $handle = [IntPtr]$button.Current.NativeWindowHandle
        [uint32]$ownerId = 0
        [void][ConfirmDialogControls]::GetWindowThreadProcessId($handle, [ref]$ownerId)
        if ($ownerId -ne $ApplicationPid -or -not [ConfirmDialogControls]::IsChild([IntPtr]$dialog.Current.NativeWindowHandle, $handle)) {
            throw 'Refusing to interact with a control outside the application dialog.'
        }
        if (-not [ConfirmDialogControls]::PostMessage($handle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)) {
            throw 'Unable to activate confirmation button.'
        }
        exit 0
    }
    Start-Sleep -Milliseconds 100
}
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$owned = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ApplicationPid)
[System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Descendants, $owned) | ForEach-Object {
    [PSCustomObject]@{ Name = $_.Current.Name; Class = $_.Current.ClassName; Id = $_.Current.AutomationId }
} | Format-Table -AutoSize | Out-String -Width 240 | Write-Output
throw 'Confirmation button not found for the test application process.'
