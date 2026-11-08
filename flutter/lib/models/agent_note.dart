import 'dart:math' as math;
import 'dart:typed_data';
import 'dart:ui' as ui;

import 'package:flutter/painting.dart';

/// The message box's tools: clip picks the region sent, the rest draw on it.
enum NoteTool { clip, pen, arrow, box, text }

/// One annotation, in the screenshot's own pixel coordinates.
class NoteMark {
  final NoteTool tool;
  final List<Offset> points;
  final String text;

  NoteMark(this.tool, this.points, {this.text = ''});
}

const noteInk = Color(0xFFE53935);

/// Draw [marks] in screenshot coordinates. [strokeScale] keeps lines as thick as they looked while drawn on a
/// scaled-down preview.
void paintMarks(Canvas canvas, List<NoteMark> marks, {double strokeScale = 1}) {
  final paint = Paint()
    ..color = noteInk
    ..style = PaintingStyle.stroke
    ..strokeWidth = 4 * strokeScale
    ..strokeCap = StrokeCap.round
    ..strokeJoin = StrokeJoin.round;
  for (final mark in marks) {
    final points = mark.points;
    if (points.isEmpty) continue;
    if (mark.tool == NoteTool.pen && points.length > 1) {
      final path = Path()..moveTo(points.first.dx, points.first.dy);
      for (final p in points.skip(1)) {
        path.lineTo(p.dx, p.dy);
      }
      canvas.drawPath(path, paint);
    } else if (mark.tool == NoteTool.box && points.length > 1) {
      canvas.drawRect(Rect.fromPoints(points.first, points.last), paint);
    } else if (mark.tool == NoteTool.arrow && points.length > 1) {
      final from = points.first;
      final to = points.last;
      canvas.drawLine(from, to, paint);
      final angle = math.atan2(to.dy - from.dy, to.dx - from.dx);
      final head = 18 * strokeScale;
      for (final side in [-0.5, 0.5]) {
        final a = angle + math.pi + side;
        canvas.drawLine(
            to, to + Offset(math.cos(a) * head, math.sin(a) * head), paint);
      }
    } else if (mark.tool == NoteTool.text && mark.text.isNotEmpty) {
      final painter = TextPainter(
        text: TextSpan(
          text: mark.text,
          style: TextStyle(
            color: noteInk,
            fontSize: 24 * strokeScale,
            fontWeight: FontWeight.bold,
          ),
        ),
        textDirection: TextDirection.ltr,
      )..layout();
      painter.paint(canvas, points.first);
    }
  }
}

/// The PNG sent with a message: [frame], cut to [clip] when one is set, with [marks] drawn onto it.
Future<Uint8List> bakeNote(ui.Image frame, Rect? clip, List<NoteMark> marks,
    {double strokeScale = 1}) async {
  final full =
      Rect.fromLTWH(0, 0, frame.width.toDouble(), frame.height.toDouble());
  var region = clip == null ? full : clip.intersect(full);
  if (region.width < 1 || region.height < 1) region = full;
  final left = region.left.roundToDouble();
  final top = region.top.roundToDouble();
  final width = math.max(1, region.width.round());
  final height = math.max(1, region.height.round());
  final recorder = ui.PictureRecorder();
  final canvas = Canvas(recorder);
  canvas.translate(-left, -top);
  canvas.drawImage(frame, Offset.zero, Paint());
  paintMarks(canvas, marks, strokeScale: strokeScale);
  final picture = recorder.endRecording();
  final image = await picture.toImage(width, height);
  picture.dispose();
  final data = await image.toByteData(format: ui.ImageByteFormat.png);
  image.dispose();
  return data == null ? Uint8List(0) : data.buffer.asUint8List();
}
