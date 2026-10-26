#!/usr/bin/env swift
// Finder background in points, matching dmg-layout.applescript. Render both
// standard and Retina representations so the arrows stay crisp on either screen.
import AppKit

guard CommandLine.arguments.count == 2 else {
    fputs("Usage: dmg-background.swift <output.tiff>\n", stderr)
    exit(1)
}

let canvas = NSSize(width: 800, height: 540)
let destination = NSPoint(x: 550, y: 270)

func color(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255,
            green: CGFloat((hex >> 8) & 255) / 255,
            blue: CGFloat(hex & 255) / 255, alpha: 1)
}

func pointer(at center: NSPoint, tint: NSColor) {
    NSGraphicsContext.saveGraphicsState()
    let transform = NSAffineTransform()
    transform.translateX(by: center.x, yBy: center.y)
    transform.rotate(byRadians: atan2(destination.y - center.y, destination.x - center.x))
    transform.concat()

    // A small round companion follows a white-edged arrowhead towards Applications.
    let arrow = NSBezierPath()
    arrow.move(to: NSPoint(x: 43, y: 0))
    arrow.line(to: NSPoint(x: 20, y: -12))
    arrow.line(to: NSPoint(x: 25, y: 0))
    arrow.line(to: NSPoint(x: 20, y: 12))
    arrow.close()
    arrow.lineJoinStyle = .round
    arrow.lineWidth = 4

    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.13)
    shadow.shadowBlurRadius = 3
    shadow.shadowOffset = NSSize(width: 0, height: 2)
    shadow.set()
    NSColor.white.setStroke()
    arrow.stroke()
    tint.blended(withFraction: 0.15, of: .white)!.setFill()
    arrow.fill()
    NSGraphicsContext.restoreGraphicsState()

    tint.setFill()
    NSBezierPath(ovalIn: NSRect(x: -13, y: -13, width: 26, height: 26)).fill()
    NSColor.white.setStroke()
    for y: CGFloat in [-4, 4] {
        let eye = NSBezierPath()
        eye.lineWidth = 3
        eye.lineCapStyle = .round
        eye.move(to: NSPoint(x: 0, y: y))
        eye.line(to: NSPoint(x: 5, y: y))
        eye.stroke()
    }
    NSGraphicsContext.restoreGraphicsState()
}

var representations: [NSBitmapImageRep] = []
for scale in [1, 2] {
    let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: Int(canvas.width) * scale,
        pixelsHigh: Int(canvas.height) * scale, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    bitmap.size = canvas
    NSGraphicsContext.saveGraphicsState()
    guard let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
        fatalError("Could not create the DMG drawing context.")
    }
    NSGraphicsContext.current = context
    let transform = NSAffineTransform()
    // NSGraphicsContext already scales pixels to bitmap.size (including Retina).
    transform.translateX(by: 0, yBy: canvas.height)
    transform.scaleX(by: 1, yBy: -1)
    transform.concat()

    let background = NSGradient(starting: color(0xFCFCFD), ending: color(0xF3F5F8))!
    background.draw(in: NSRect(origin: .zero, size: canvas), angle: 90)
    for (x, y, hex): (CGFloat, CGFloat, UInt32) in [
        (520, 62, 0xFF4058),
        (367, 137, 0xA27A51),
        (730, 138, 0x18BDAA),
        (728, 425, 0x2789FF),
        (490, 490, 0xFF8A35),
        (365, 389, 0x9862FF),
    ] {
        pointer(at: NSPoint(x: x, y: y), tint: color(hex))
    }
    NSGraphicsContext.restoreGraphicsState()
    representations.append(bitmap)
}

guard let data = NSBitmapImageRep.representationOfImageReps(
    in: representations, using: .tiff,
    properties: [.compressionMethod: NSBitmapImageRep.TIFFCompression.lzw.rawValue]
) else {
    fputs("Could not encode the DMG background.\n", stderr)
    exit(1)
}
try data.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
