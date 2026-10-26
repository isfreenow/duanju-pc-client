Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$res=@()
$root=[Windows.Automation.AutomationElement]::RootElement
$tb=$root.FindAll([Windows.Automation.TreeScope]::Children,[Windows.Automation.Condition]::TrueCondition)
foreach($w in $tb){
 if($w.Current.ClassName -eq "Shell_TrayWnd"){
  $items=$w.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
  foreach($b in $items){ $n=$b.Current.Name; if($n){ $r=$b.Current.BoundingRectangle; $res += ($b.Current.ControlType.ProgrammaticName + " | " + $n + " | " + [int]$r.X + "," + [int]$r.Y + " " + [int]$r.Width + "x" + [int]$r.Height) } }
 }
}
$res | Set-Content -Path "D:\workspace\Codespace\duanju-pc-client\.tmp-shot\taskbar_items.txt" -Encoding UTF8
Write-Output ("items=" + $res.Count)
