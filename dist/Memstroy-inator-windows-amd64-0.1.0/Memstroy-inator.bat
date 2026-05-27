@echo off
REM Launch the Memstroy-inator editor.
REM
REM All assets are fetched from the configured assets-server on demand
REM and cached under %USERPROFILE%\.memstroy\cache\.
"%~dp0bin\memstroy-gui.exe" %*
