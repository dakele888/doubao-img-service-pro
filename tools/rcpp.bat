@echo off
rem Preprocessor stub for GNU windres: copies the .rc file as-is.
rem Usage: rcpp.bat <input.rc> [-o <output>]
setlocal
set "in="
set "out="
:parse
if "%~1"=="" goto run
if /i "%~1"=="-o" (
    set "out=%~2"
    shift
    shift
    goto parse
)
if not defined in set "in=%~1"
shift
goto parse
:run
if not defined in exit /b 0
if not defined out goto stdout
if /i "%out%"=="-" goto stdout
type "%in%" > "%out%"
exit /b 0
:stdout
type "%in%"
exit /b 0
