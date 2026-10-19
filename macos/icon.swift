// Draws the app icon: one connection split across three links and joined again.
//   swift macos/icon.swift out.png   (1024x1024; build-macos-app.sh turns it into AppIcon.icns)
import AppKit

let size = 1024.0
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

func rgb(_ hex: Int, _ alpha: Double = 1) -> NSColor {
    NSColor(srgbRed: Double(hex >> 16 & 0xff) / 255, green: Double(hex >> 8 & 0xff) / 255,
            blue: Double(hex & 0xff) / 255, alpha: alpha)
}

// macOS icon grid: an 824pt rounded square centred on the 1024 canvas, with a soft drop shadow.
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let shape = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
NSGraphicsContext.saveGraphicsState()
let shadow = NSShadow()
shadow.shadowColor = rgb(0x000000, 0.35)
shadow.shadowOffset = NSSize(width: 0, height: -12)
shadow.shadowBlurRadius = 28
shadow.set()
rgb(0x0b1e3f).setFill()
shape.fill()
NSGraphicsContext.restoreGraphicsState()
NSGradient(colors: [rgb(0x1d4ed8), rgb(0x0b1e3f)])!.draw(in: shape, angle: -90)

// Three links fanning out from the left node and meeting at the right one.
let left = NSPoint(x: 300, y: 512), right = NSPoint(x: 724, y: 512)
for (bend, color) in [(230.0, 0x34d399), (0.0, 0x38bdf8), (-230.0, 0xa78bfa)] {
    let path = NSBezierPath()
    path.move(to: left)
    path.curve(to: right, controlPoint1: NSPoint(x: 420, y: 512 + bend * 1.3),
               controlPoint2: NSPoint(x: 604, y: 512 + bend * 1.3))
    path.lineWidth = 46
    path.lineCapStyle = .round
    rgb(color).setStroke()
    path.stroke()
}
for p in [left, right] {
    let r = 64.0
    let dot = NSBezierPath(ovalIn: NSRect(x: p.x - r, y: p.y - r, width: 2 * r, height: 2 * r))
    rgb(0xffffff).setFill()
    dot.fill()
}

NSGraphicsContext.current = nil
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
