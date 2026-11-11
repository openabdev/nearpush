import CoreGraphics
import CoreImage
import CoreImage.CIFilterBuiltins

public enum QRImage {
    /// A QR code for `text`, about `points` wide (2x pixels). Nil for empty or unencodable text.
    public static func cgImage(for text: String, points: CGFloat = 240) -> CGImage? {
        guard !text.isEmpty else { return nil }
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage, out.extent.width > 0 else { return nil }
        let scale = max(1, (points * 2 / out.extent.width).rounded(.down)) // 2x for Retina
        let scaled = out.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        return CIContext().createCGImage(scaled, from: scaled.extent)
    }
}
