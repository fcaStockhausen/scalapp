#!/bin/bash
# Script de activación del entorno virtual para macOS/Linux

echo "Activando entorno virtual..."

if [ -d "venv" ]; then
    source venv/bin/activate
    echo "✓ Entorno virtual activado"
    echo ""
    echo "Para ejecutar el proyecto:"
    echo "  python main.py --list-devices        # Listar dispositivos"
    echo "  python main.py --audio-only          # Audio-only (~6-8ms)"
    echo "  python main.py                      # Audio + Video completo"
    echo ""
    echo "Para desactivar: deactivate"
else
    echo "✗ Error: El directorio 'venv' no existe"
    echo "  Ejecuta primero: python3 -m venv venv"
    exit 1
fi
