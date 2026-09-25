@echo off
REM Build the C demo and link it against the Rust core (staticlib).
setlocal

call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 (
    echo [ERROR] vcvars64.bat failed
    exit /b 1
)

cd /d "%~dp0"
cl /nologo /O2 /utf-8 /Fe:demo.exe /I..\include demo.c ..\target\release\downloadcore.lib ^
   ws2_32.lib userenv.lib bcrypt.lib ntdll.lib advapi32.lib ole32.lib shell32.lib crypt32.lib
if errorlevel 1 (
    echo [ERROR] compile/link failed
    exit /b 1
)
echo [OK] built demo.exe
