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

## 4. Leads y ventas

Cada proyecto tiene una pestaña **Leads** para seguir a la gente a la que le
estás vendiendo.

- **+ → Desde un chat** suma a alguien con quien ya hablás; **+ → Por número**
  pega una lista (`Nombre, +54 9 11 …`, uno por línea) de gente a la que le vas
  a escribir: arrancan como Nuevo, con «Primer mensaje» para hoy.
- Cada lead tiene una **etapa** (Nuevo, Contactado, Cotizado, Cliente,
  Perdido; Perdido pide el motivo), una **próxima acción** con día (Hoy,
  Mañana, En 3 días, Próxima semana) y un **historial** de notas, llamadas y
  seguimientos, escritos o dictados con el micrófono. **Resumir el chat y
  anotarlo** le pide al motor de análisis un resumen del último mes del chat.
- La lista se ordena por lo que hay que hacer: atrasados, para hoy, sin
  próxima acción (los que se enfrían) y próximos. Clientes y perdidos quedan
  plegados al final. ✓ marca la acción como hecha; el reloj la pospone.
- **Mensajes guardados**: textos de primer contacto con `{name}`. Un clic los
  pone en el chat con el nombre; nunca se mandan solos.
- En el **Panel** del proyecto aparece la tarjeta **Ventas** (atrasados, para
  hoy, cotizados, clientes).

### Conectar con ElevaLeads

**Leads → Conectar ElevaLeads** y pegá un token de ElevaLeads limitado a una
marca (ElevaLeads → Configuración → Conectar con IA). Whats Pro dice a qué
marca quedó conectado y si el token es de solo lectura o de lectura y edición.

- En el **Panel** aparece **WhatsApp ‹marca› (empresa)**: quién espera a una
  persona, las situaciones para atender y la agenda de la marca. Se lee cada
  5 minutos. Cada ítem se abre en ElevaLeads y, con un token de edición, se
  marca como atendido (no le manda nada al cliente).
- En un lead, **Vincular** busca en la marca al cliente con el mismo número.
  Desde ahí, con un token de edición, cada nota, llamada y resumen se suma a
  su ficha, el cambio de etapa se refleja allá (Contactado = En conversación,
  Cotizado = Interesado, Perdido = Dormido) y la próxima acción queda como
  situación en la bandeja del equipo. Con solo lectura, todo queda guardado en
  Whats Pro y se puede volver a enviar después.

### Ventas cerradas por WhatsApp

En la ficha de cada lead, **Pedidos en ‹marca›** muestra lo que ElevaLeads sabe
de ese teléfono: cuántos pedidos tiene, cuánto compró y los últimos pedidos.
Si el lead está en **Cliente** y en ElevaLeads no tiene ningún pedido, es una
venta que se cerró solo por WhatsApp: la ficha lo avisa, y en el Panel aparece
«Vendidos por WhatsApp sin registrar».

**Registrar venta** carga en ElevaLeads a la clienta (si no existe) y su
pedido: qué compró, el total, el estado (por defecto *Cerrado*) y el pago (por
defecto *Pagado*). Antes de mandarla, Whats Pro dice si con ese estado
ElevaLeads le manda a la clienta un aviso automático por WhatsApp. Hace falta
una clave de API de la marca: con un token de edición, **Crear una clave para
Whats Pro** la genera y la guarda en el llavero; si no, se puede pegar una
(`eleva_…`, ElevaLeads → Integraciones).

## Privacidad

- Con el motor desactivado, nada sale de tu Mac.
- El análisis manda al proveedor que elijas la última semana de mensajes de
  las fuentes de tus proyectos. Los chats personales, los ignorados y los que
  no están en ningún proyecto nunca se mandan. Tampoco se mandan los mensajes
  que marcaste como que no son del proyecto.
- Las keys quedan en el llavero de macOS. Las herramientas instaladas quedan
  en `~/Library/Application Support/me.paolino.zapfast-julito/tools`, salvo
  Claude Code, que se instala en `~/.local/bin`.
- Los leads quedan en `leads.json`, en la carpeta de configuración. Los tokens
  de ElevaLeads quedan en el llavero de macOS, uno por marca. A ElevaLeads solo
  va lo que anotás en un lead vinculado y los pedidos para leer la bandeja de
  su marca; los mensajes del chat no se mandan, salvo el resumen que vos
  pedís.
