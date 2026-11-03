# Native reads avoid parsing localized powercfg output (which also contains option values).
if (-not ('Arcturus.LidNative' -as [type])) {
    $lidCompileTemp = Join-Path $env:TEMP 'away-guard-lid'
    New-Item -ItemType Directory -Path $lidCompileTemp -Force | Out-Null
    $lidOldTemp = $env:TEMP
    $lidOldTmp = $env:TMP
    try {
    $env:TEMP = $lidCompileTemp
    $env:TMP = $lidCompileTemp
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace Arcturus {
  public static class LidNative {
    [DllImport("powrprof.dll")] static extern uint PowerGetActiveScheme(IntPtr root, out IntPtr scheme);
    [DllImport("powrprof.dll")] static extern uint PowerReadACValueIndex(IntPtr root, ref Guid scheme, ref Guid subgroup, ref Guid setting, out uint value);
    [DllImport("powrprof.dll")] static extern uint PowerReadDCValueIndex(IntPtr root, ref Guid scheme, ref Guid subgroup, ref Guid setting, out uint value);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr pointer);
    [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint flags);
    public static string[] Read() {
      IntPtr pointer; uint code=PowerGetActiveScheme(IntPtr.Zero,out pointer);
      if(code!=0) throw new InvalidOperationException("PowerGetActiveScheme: "+code);
      Guid scheme; try {scheme=(Guid)Marshal.PtrToStructure(pointer,typeof(Guid));} finally {LocalFree(pointer);}
      Guid group=new Guid("4f971e89-eebd-4455-a8de-9e59040e7347");
      Guid lid=new Guid("5ca83367-6e45-459f-a27b-476b1d01c936");
      uint ac,dc; code=PowerReadACValueIndex(IntPtr.Zero,ref scheme,ref group,ref lid,out ac);
      if(code!=0) throw new InvalidOperationException("PowerReadACValueIndex: "+code);
      code=PowerReadDCValueIndex(IntPtr.Zero,ref scheme,ref group,ref lid,out dc);
      if(code!=0) throw new InvalidOperationException("PowerReadDCValueIndex: "+code);
      return new string[]{scheme.ToString(),ac.ToString(),dc.ToString()};
    }
  }
}
'@
    } finally {
        $env:TEMP = $lidOldTemp
        $env:TMP = $lidOldTmp
    }
}
