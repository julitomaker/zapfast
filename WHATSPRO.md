# Whats Pro: guía de uso

Whats Pro es un cliente de WhatsApp para Mac con una página de **Proyectos**:
agrupa tus chats y grupos por proyecto y arma un panel con lo que tenés que
hacer, a quién darle seguimiento, qué se está cayendo y un resumen por día.

## 1. Vincular tu WhatsApp

1. Abrí Whats Pro. Aparece un código QR.
2. En el teléfono: WhatsApp → Ajustes → Dispositivos vinculados → Vincular un
   dispositivo, y escaneá el código.
3. Se bajan tus chats y hasta un año de historial. La primera vez puede tardar
   unos minutos.

## 2. Armar tus proyectos

En la barra de la izquierda, el ícono de etiqueta abre **Proyectos**.

- **+** crea un proyecto y abre **Fuentes** para sumarle chats.
- Cada fuente tiene un modo:
  - **Todo**: entra el chat completo. Es el modo de los grupos que son solo de
    ese proyecto.
  - **Inteligente**: el análisis se queda solo con los mensajes del proyecto.
    Es el modo de los chats con una persona y de los chats que están en más de
    un proyecto. Whats Pro avisa cuando un chat también habla de otras cosas.
  - **Elegidos**: entran solo los mensajes que marcás a mano.
- En **Mensajes** podés seleccionar mensajes y marcarlos como de este proyecto
  o de otro, o como que no son de ninguno. Esas elecciones mandan siempre, y
  el análisis aprende de ellas.
- En **Organizar → Sin asignar** están los chats que todavía no ubicaste.
  Personal e Ignorados quedan fuera de todo.

## 3. Activar el análisis con IA

**Proyectos → Organizar → Análisis con IA.** Elegí un motor:

**Con tu suscripción (sin API key):**

| Motor | Cuenta | Qué hace Whats Pro |
|---|---|---|
| Claude | Claude Pro o Max | Instala Claude Code |
| ChatGPT · Codex | ChatGPT Plus o Pro | Instala Codex CLI |
| Gemini | Cuenta de Google | Instala Gemini CLI (y Node.js si hace falta) |

Para cada uno, la página guía tres pasos:

1. **Instalar**: lo baja de su fabricante. No pide contraseña de administrador
   ni necesita Homebrew.
2. **Iniciar sesión**: abre la Terminal en el inicio de sesión de la
   herramienta. Entrás con tu cuenta en el navegador y cerrás la ventana. Es
   una sola vez.
3. **Probar conexión**: confirma que responde.

**Con API key (se paga por uso):** Anthropic, OpenAI u OpenRouter. Pegás la key
(queda guardada en el llavero de macOS) y probás la conexión. El link
"Conseguir una key" lleva a la página donde se crean.

El **modelo** se elige de una lista: el recomendado aparece primero, con una
nota de para qué sirve cada uno. Las API también muestran todos sus modelos, y
podés escribir el nombre de cualquier otro.

Con **Cada cuánto** elegís cada cuánto se revisan los proyectos con mensajes
nuevos. **Analizar ahora** lo corre en el momento.

## Privacidad

- Con el motor desactivado, nada sale de tu Mac.
- El análisis manda al proveedor que elijas la última semana de mensajes de
  las fuentes de tus proyectos. Los chats personales, los ignorados y los que
  no están en ningún proyecto nunca se mandan. Tampoco se mandan los mensajes
  que marcaste como que no son del proyecto.
- Las keys quedan en el llavero de macOS. Las herramientas instaladas quedan
  en `~/Library/Application Support/me.paolino.zapfast-julito/tools`, salvo
  Claude Code, que se instala en `~/.local/bin`.
