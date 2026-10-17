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
let source = catalogIcon(app: arguments[1]) ?? NSWorkspace.shared.icon(forFile: arguments[1])
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

    // A dark pill across the lower edge of the squircle (macOS icons sit in
    // 824 points centred on the 1024 canvas, so its bottom is at y = 100).
    let pill = NSRect(x: 512 - 200, y: 42, width: 400, height: 150)
    let path = NSBezierPath(roundedRect: pill, xRadius: 75, yRadius: 75)

    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.35)
    shadow.shadowBlurRadius = 24
    shadow.shadowOffset = NSSize(width: 0, height: -8)
    shadow.set()
    NSColor(srgbRed: 0.07, green: 0.11, blue: 0.13, alpha: 1).setFill()
    path.fill()
    NSGraphicsContext.restoreGraphicsState()

    // A soft top-lit sheen, then a white rim that separates it from the green.
    NSGraphicsContext.saveGraphicsState()
    path.addClip()
    NSGradient(colors: [
        NSColor(srgbRed: 0.20, green: 0.25, blue: 0.27, alpha: 1),
        NSColor(srgbRed: 0.05, green: 0.08, blue: 0.10, alpha: 1),
    ])!.draw(in: pill, angle: -90)
    NSGraphicsContext.restoreGraphicsState()
    NSColor.white.setStroke()
    path.lineWidth = 12
    path.stroke()

    // "PRO" in the system's rounded heavy face, WhatsApp green.
    var font = NSFont.systemFont(ofSize: 100, weight: .heavy)
    if let rounded = font.fontDescriptor.withDesign(.rounded) {
        font = NSFont(descriptor: rounded, size: 100) ?? font
    }
    let text = NSAttributedString(string: "PRO", attributes: [
        .font: font,
        .foregroundColor: NSColor(srgbRed: 0.145, green: 0.827, blue: 0.400, alpha: 1),
        .kern: 9,
    ])
    let size = text.size()
    text.draw(at: NSPoint(x: pill.midX - size.width / 2 + 4.5,
                          y: pill.midY - size.height / 2 + 3))

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
