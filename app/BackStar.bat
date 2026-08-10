@echo off
rem BackStar launcher - hands off to the VBScript wrapper so no console window
rem stays open behind the app. Double-click BackStar.vbs for zero console flash.
wscript.exe "%~dp0BackStar.vbs"
