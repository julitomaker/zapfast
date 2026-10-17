// Draws an app's icon with a "PRO" badge and writes it as an .icns.
//
//   swift packaging/macos/pro-icon.swift <source.app> <output.icns>
//
// The source icon is rendered from the installed app on this computer at build
// time; nothing of it is stored in this repository.
import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    FileHandle.standardError.write("usage: pro-icon.swift <source.app> <output.icns>\n".data(using: .utf8)!)
    exit(2)
}
/// The icon's 1024-pixel rendition from the app's asset catalog, read through
/// CoreUI (the catalog behind `NSImage(named:)`), or the workspace icon.
func catalogIcon(app: String) -> NSImage? {
    let car = URL(fileURLWithPath: app).appendingPathComponent("Contents/Resources/Assets.car")
    guard let type = NSClassFromString("CUICatalog") as? NSObject.Type else { return nil }
    let allocated = type.perform(NSSelectorFromString("alloc"))!.takeUnretainedValue()
    guard let catalog = allocated.perform(
        NSSelectorFromString("initWithURL:error:"), with: car, with: nil
    )?.takeUnretainedValue() as? NSObject else { return nil }
    let names = catalog.value(forKey: "allImageNames") as? [String] ?? []
    guard names.contains("AppIcon") else { return nil }
    let images = catalog.perform(NSSelectorFromString("imagesWithName:"), with: "AppIcon")?
        .takeUnretainedValue() as? [NSObject] ?? []
    let best = images
        .compactMap { image -> CGImage? in
            let selector = NSSelectorFromString("image")
            guard image.responds(to: selector),
                  let cg = image.perform(selector)?.takeUnretainedValue() else { return nil }
            return (cg as! CGImage)
        }
        .max { $0.width < $1.width }
    return best.map { NSImage(cgImage: $0, size: NSSize(width: $0.width, height: $0.height)) }
}
/// The icon exactly as macOS draws it for the app (its layered icon rendered
/// by the system), so the logo keeps the same size; the catalog's flat
/// rendition is the fallback.
func systemIcon(app: String) -> NSImage? {
    _ = NSApplication.shared
    let icon = NSWorkspace.shared.icon(forFile: app)
    // The system renders layered icons asynchronously.
    RunLoop.current.run(until: Date().addingTimeInterval(2))
    icon.size = NSSize(width: 1024, height: 1024)
    return icon
}
let source = systemIcon(app: arguments[1]) ?? catalogIcon(app: arguments[1])!
let output = URL(fileURLWithPath: arguments[2])

/// The icon at `side` pixels, drawn at 1024 and scaled so the badge keeps its
/// proportions at every size.
func render(side: Int) -> Data {
    let canvas = 1024.0
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: side, pixelsHigh: side,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: canvas, height: canvas)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high

    source.draw(in: NSRect(x: 0, y: 0, width: canvas, height: canvas),
                from: .zero, operation: .sourceOver, fraction: 1)

    // A pill inside the squircle's lower edge. macOS 26 shrinks an icon into
    // a grey tile when anything sticks out of the standard squircle (824
    // points centred on the 1024 canvas), so the badge stays well inside it,
    // under the logo and clear of the bubble's tail.
    let dark = ProcessInfo.processInfo.environment["PRO_STYLE"] == "dark"
    let pill = NSRect(x: 512 - 128, y: 116, width: 256, height: 92)
    let path = NSBezierPath(roundedRect: pill, xRadius: 46, yRadius: 46)
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.18)
    shadow.shadowBlurRadius = 14
    shadow.shadowOffset = NSSize(width: 0, height: -4)
    shadow.set()
    (dark ? NSColor.black.withAlphaComponent(0.28) : NSColor.white).setFill()
    path.fill()
    NSGraphicsContext.restoreGraphicsState()

    var font = NSFont.systemFont(ofSize: 60, weight: .heavy)
    if let rounded = font.fontDescriptor.withDesign(.rounded) {
        font = NSFont(descriptor: rounded, size: 60) ?? font
    }
    let text = NSAttributedString(string: "PRO", attributes: [
        .font: font,
        .foregroundColor: dark
            ? NSColor.white
            : NSColor(srgbRed: 0.08, green: 0.66, blue: 0.33, alpha: 1),
        .kern: 6,
    ])
    let size = text.size()
    text.draw(at: NSPoint(x: pill.midX - size.width / 2 + 3,
                          y: pill.midY - size.height / 2 + 1.5))

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

let iconset = FileManager.default.temporaryDirectory
    .appendingPathComponent("pro-\(ProcessInfo.processInfo.processIdentifier).iconset")
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    try render(side: base).write(to: iconset.appendingPathComponent("icon_\(base)x\(base).png"))
    try render(side: base * 2).write(to: iconset.appendingPathComponent("icon_\(base)x\(base)@2x.png"))
}
let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", output.path]
try iconutil.run()
iconutil.waitUntilExit()
try? FileManager.default.removeItem(at: iconset)
exit(iconutil.terminationStatus)
