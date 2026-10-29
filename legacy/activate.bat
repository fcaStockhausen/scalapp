@echo off
REM Script de activación del entorno virtual para Windows

echo Activando entorno virtual...

if exist "venv\Scripts\activate.bat" (
    call venv\Scripts\activate.bat
    echo.
    echo [+] Entorno virtual activado
    echo.
    echo Para ejecutar el proyecto:
    echo   python main.py --list-devices        # Listar dispositivos
    echo   python main.py --audio-only          # Audio-only (~6-8ms)
    echo   python main.py                      # Audio + Video completo
    echo.
    echo Para desactivar: deactivate
) else (
    echo [x] Error: El directorio 'venv' no existe
    echo   Ejecuta primero: python -m venv venv
    exit /b 1
)
