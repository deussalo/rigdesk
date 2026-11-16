import 'package:flutter/material.dart';

import '../common.dart' show ColorThemeExtension;
import '../desktop/widgets/tabbar_widget.dart' show TabbarTheme;

/// Rig's look: the tokens of Rig's web/src/ui/theme.css, named as there, and
/// the one dark theme built from them.
class RigTheme {
  RigTheme._();

  static const bg = Color(0xFF1C1C1C);
  static const surface = Color(0xFF242424);
  static const text = Color(0xFFCBCBB8);
  static const textDim = Color(0xFF8A8A8A);
  static const accent = Color(0xFFBB7744);
  static const gold = Color(0xFFC9A554);
  static const green = Color(0xFF5F875F);
  static const red = Color(0xFFB04848);
  static const selectedFg = Color(0xFF000000);

  // theme.css derives these with oklch() and color-mix(); the values are
  // Chromium's resolution of those formulas.
  static const surfaceRaised = Color(0xFF303030);
  static const rowHover = Color(0xFF2A2A2A);
  static const textMid = Color(0xFFB8B8AB);
  static const textBright = Color(0xFFE5E5D2);
  static const textFaint = Color(0xFF555555);
  static const border = Color(0xFF484848);
  static const link = Color(0xFFD8925F);

  static const toneIdle = textDim;
  static const toneLive = green;
  static const toneHold = gold;
  static const toneFault = red;
  static const toneLiveBg = Color(0xFF262C26);
  static const toneHoldBg = Color(0xFF363124);
  static const toneFaultBg = Color(0xFF2B2020);
  static const toneLiveInk = Color(0xFF9AAC8F);
  static const toneHoldInk = Color(0xFFCBBA8E);
  static const toneFaultInk = Color(0xFFC49284);

  static const overlaySoft = Color(0x0DCBCBB8);
  static const overlaySoftStrong = Color(0x1ACBCBB8);
  static const overlayAccent = Color(0x29BB7744);
  static const accent50 = Color(0x77BB7744);
  static const accent80 = Color(0xAABB7744);

  /// RustDesk's dark theme with every colour taken from the tokens.
  static ThemeData apply(ThemeData upstream) {
    final scheme = ColorScheme.dark(
      primary: accent,
      onPrimary: bg,
      secondary: accent,
      onSecondary: bg,
      surface: surface,
      onSurface: text,
      background: bg,
      onBackground: text,
      error: toneFault,
      onError: text,
    );
    return upstream.copyWith(
      colorScheme: scheme,
      primaryColor: accent,
      canvasColor: surface,
      scaffoldBackgroundColor: bg,
      dialogBackgroundColor: surface,
      cardColor: surface,
      hoverColor: rowHover,
      focusColor: overlayAccent,
      highlightColor: upstream.highlightColor == Colors.transparent
          ? Colors.transparent
          : overlaySoft,
      dividerColor: border,
      hintColor: textDim,
      disabledColor: textFaint,
      unselectedWidgetColor: textDim,
      indicatorColor: accent,
      shadowColor: Colors.black,
      textTheme: upstream.textTheme.apply(
        bodyColor: text,
        displayColor: text,
      ),
      iconTheme: upstream.iconTheme.copyWith(color: text),
      primaryIconTheme: upstream.primaryIconTheme.copyWith(color: text),
      textSelectionTheme: TextSelectionThemeData(
        cursorColor: accent,
        selectionColor: overlayAccent,
        selectionHandleColor: accent,
      ),
      appBarTheme: upstream.appBarTheme.copyWith(
        backgroundColor: bg,
        foregroundColor: text,
      ),
      dialogTheme: upstream.dialogTheme.copyWith(
        backgroundColor: surface,
        shape: _withSide(upstream.dialogTheme.shape, BorderSide(color: border)),
      ),
      inputDecorationTheme: upstream.inputDecorationTheme.copyWith(
        fillColor: overlaySoftStrong,
        hintStyle: TextStyle(color: textDim),
        labelStyle: TextStyle(color: textDim),
        floatingLabelStyle: TextStyle(color: accent),
        focusColor: accent,
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(foregroundColor: text)
            .merge(upstream.textButtonTheme.style),
      ),
      elevatedButtonTheme: ElevatedButtonThemeData(
        style: ElevatedButton.styleFrom(
          backgroundColor: accent,
          foregroundColor: bg,
          disabledBackgroundColor: overlaySoftStrong,
          disabledForegroundColor: textFaint,
        ).merge(upstream.elevatedButtonTheme.style),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          backgroundColor: surfaceRaised,
          foregroundColor: text,
          disabledForegroundColor: textFaint,
          side: BorderSide.none,
        ).merge(upstream.outlinedButtonTheme.style),
      ),
      switchTheme: upstream.switchTheme.copyWith(
        thumbColor: _onOff(accent, textDim),
        trackColor: _onOff(overlayAccent, overlaySoftStrong),
        trackOutlineColor: const MaterialStatePropertyAll(Colors.transparent),
      ),
      checkboxTheme: upstream.checkboxTheme.copyWith(
        fillColor: _onOff(accent, Colors.transparent),
        checkColor: const MaterialStatePropertyAll(bg),
        side: BorderSide(color: textDim, width: 1.5),
      ),
      radioTheme: upstream.radioTheme.copyWith(fillColor: _onOff(accent, textDim)),
      tabBarTheme: upstream.tabBarTheme.copyWith(
        labelColor: text,
        unselectedLabelColor: textDim,
        indicatorColor: accent,
      ),
      listTileTheme: upstream.listTileTheme.copyWith(
        iconColor: text,
        textColor: text,
        selectedColor: accent,
        selectedTileColor: surfaceRaised,
      ),
      popupMenuTheme: upstream.popupMenuTheme.copyWith(
        color: surface,
        shape: _withSide(upstream.popupMenuTheme.shape, BorderSide(color: border)),
      ),
      menuBarTheme: MenuBarThemeData(
          style: MenuStyle(backgroundColor: MaterialStatePropertyAll(surface))),
      menuTheme: MenuThemeData(
          style: MenuStyle(backgroundColor: MaterialStatePropertyAll(surface))),
      tooltipTheme: upstream.tooltipTheme.copyWith(
        decoration: BoxDecoration(color: surfaceRaised),
        textStyle: TextStyle(color: text),
      ),
      snackBarTheme: SnackBarThemeData(
        backgroundColor: surfaceRaised,
        contentTextStyle: TextStyle(color: text),
        actionTextColor: accent,
      ),
      progressIndicatorTheme: ProgressIndicatorThemeData(color: accent),
      bottomNavigationBarTheme: BottomNavigationBarThemeData(
        backgroundColor: surface,
        selectedItemColor: accent,
        unselectedItemColor: textDim,
      ),
      extensions: <ThemeExtension<dynamic>>[
        ColorThemeExtension(
          border: border,
          border2: textDim,
          border3: border,
          highlight: surfaceRaised,
          drag_indicator: textDim,
          shadow: Colors.black,
          errorBannerBg: toneFaultBg,
          me: toneLive,
          toastBg: surfaceRaised,
          toastText: text,
          divider: border,
        ),
        TabbarTheme(
          selectedTabIconColor: accent,
          unSelectedTabIconColor: textDim,
          selectedTextColor: text,
          unSelectedTextColor: textDim,
          selectedIconColor: text,
          unSelectedIconColor: textDim,
          dividerColor: border,
          hoverColor: overlaySoft,
          closeHoverColor: overlaySoftStrong,
          selectedTabBackgroundColor: surface,
        ),
      ],
    );
  }

  static MaterialStateProperty<Color?> _onOff(Color on, Color off) =>
      MaterialStateProperty.resolveWith((states) =>
          states.contains(MaterialState.selected) ? on : off);

  static ShapeBorder? _withSide(ShapeBorder? shape, BorderSide side) =>
      shape is OutlinedBorder ? shape.copyWith(side: side) : shape;
}
