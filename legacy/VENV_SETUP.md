# Python AV Monitor - Virtual Environment Setup

## Activar el entorno virtual

### macOS/Linux
```bash
source venv/bin/activate
```

### Windows (PowerShell)
```powershell
.\venv\Scripts\Activate.ps1
```

### Windows (CMD)
```cmd
venv\Scripts\activate.bat
```

## Ejecutar el proyecto

### Una vez activado el venv:
```bash
# Listar dispositivos disponibles
python main.py --list-devices

# Audio-only con latencia mínima (Direct Passthrough ~6-8ms)
python main.py --audio-only

# Audio-only con más estabilidad (~20-30ms)
python main.py --audio-only --no-direct-passthrough

# Audio + video completo
python main.py
```

## Desactivar el entorno virtual

```bash
deactivate
```

## Dependencias instaladas

- opencv-python >= 4.8.0
- sounddevice >= 0.4.5
- numpy >= 1.24.0
- PyQt6 >= 6.5.0
- av >= 10.0.0

## Reinstalar dependencias

```bash
source venv/bin/activate  # Activar primero
pip install -r requirements.txt
```

## Nota

El entorno virtual (venv) permite tener dependencias aisladas para este proyecto, sin afectar otros proyectos o el sistema principal.
