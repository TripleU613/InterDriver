@echo off
rem Export the attached Apple device from this Windows host over USB/IP.
rem The device must already be bound to WinUSB.
setlocal

set PORT=%2
if "%PORT%"=="" set PORT=3240

set BIN=%~dp0..\target\release\interdriver-usb.exe
if not exist "%BIN%" set BIN=%~dp0..\target\debug\interdriver-usb.exe
if not exist "%BIN%" (
  echo build first: cargo build --release -p interdriver-usb
  exit /b 1
)

set BUS_ID=%1
if "%BUS_ID%"=="" (
  echo Attached WinUSB devices:
  "%BIN%" windows list
  echo.
  echo Re-run with a bus id:  %~nx0 ^<bus-id^> [port]
  exit /b 1
)

echo exporting %BUS_ID% on port %PORT%
"%BIN%" windows export %BUS_ID% %PORT%
