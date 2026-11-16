import 'package:flutter/material.dart';

import '../common.dart' show ColorThemeExtension, isMacOS, isWindows;
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

  /// The platform's own monospace; no font is bundled. A family the platform
  /// lacks is not skipped for the next one in the list on every engine, so the
  /// first one named is the one this platform has.
  static final font = isWindows
      ? 'Consolas'
      : isMacOS
          ? 'Menlo'
          : 'monospace';
  static const fontFallback = ['Consolas', 'Menlo', 'monospace'];
  static const fontXs = 10.0;
  static const fontSm = 12.0;
  static const fontMd = 16.0;
  static const regular = FontWeight.w400;
  static const strong = FontWeight.w600;

  static TextTheme _type(TextTheme t) {
    TextStyle? title(TextStyle? s) => s?.copyWith(fontSize: fontMd, fontWeight: strong);
    TextStyle? size(TextStyle? s, double px) => s?.copyWith(fontSize: px, fontWeight: regular);
    return t
        .copyWith(
          displayLarge: title(t.displayLarge),
          displayMedium: title(t.displayMedium),
          displaySmall: title(t.displaySmall),
          headlineLarge: title(t.headlineLarge),
          headlineMedium: title(t.headlineMedium),
          headlineSmall: title(t.headlineSmall),
          titleLarge: title(t.titleLarge),
          titleMedium: size(t.titleMedium, fontSm),
          titleSmall: size(t.titleSmall, fontSm),
          bodyLarge: size(t.bodyLarge, fontSm),
          bodyMedium: size(t.bodyMedium, fontSm),
          bodySmall: size(t.bodySmall, fontXs),
          labelLarge: t.labelLarge?.copyWith(fontSize: fontSm, fontWeight: strong),
          labelMedium: size(t.labelMedium, fontXs),
          labelSmall: size(t.labelSmall, fontXs),
        )
        .apply(
          fontFamily: font,
          fontFamilyFallback: fontFallback,
          bodyColor: text,
          displayColor: text,
        );
  }

  static const square = RoundedRectangleBorder();
  static const focusRing = BorderSide(color: accent, width: 2);

  static InputBorder _field(BorderSide side) =>
      OutlineInputBorder(borderRadius: BorderRadius.zero, borderSide: side);

  /// Rig's Action: flat and square, with a 2 px ring when focused.
  static ButtonStyle _action(BorderSide ring) => ButtonStyle(
        shape: const WidgetStatePropertyAll(square),
        elevation: const WidgetStatePropertyAll(0),
        side: WidgetStateProperty.resolveWith((states) =>
            states.contains(WidgetState.focused) ? ring : BorderSide.none),
      );

  /// RustDesk's dark theme with every colour taken from the tokens and every
  /// corner square.
  static ThemeData apply(ThemeData upstream) {
    final scheme = ColorScheme.dark(
      primary: accent,
      onPrimary: bg,
      secondary: accent,
      onSecondary: bg,
      surface: surface,
      onSurface: text,
      error: toneFault,
      onError: text,
    );
    final type = _type(upstream.textTheme);
    final body = type.bodyMedium!;
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
      textTheme: type,
      primaryTextTheme: _type(upstream.primaryTextTheme),
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
        elevation: 0,
        shape: square.copyWith(side: BorderSide(color: border)),
      ),
      cardTheme: upstream.cardTheme.copyWith(
        color: surface,
        elevation: 0,
        shape: square,
      ),
      chipTheme: upstream.chipTheme.copyWith(shape: square),
      bottomSheetTheme: upstream.bottomSheetTheme.copyWith(
        backgroundColor: surface,
        shape: square,
      ),
      floatingActionButtonTheme: upstream.floatingActionButtonTheme.copyWith(
        backgroundColor: accent,
        foregroundColor: bg,
        shape: square,
      ),
      scrollbarTheme:
          upstream.scrollbarTheme.copyWith(radius: Radius.zero),
      inputDecorationTheme: upstream.inputDecorationTheme.copyWith(
        filled: true,
        fillColor: overlaySoftStrong,
        border: _field(BorderSide.none),
        enabledBorder: _field(BorderSide.none),
        disabledBorder: _field(BorderSide.none),
        focusedBorder: _field(focusRing),
        errorBorder: _field(const BorderSide(color: toneFault)),
        focusedErrorBorder: _field(const BorderSide(color: toneFault, width: 2)),
        hintStyle: body.copyWith(color: textDim),
        labelStyle: body.copyWith(color: textDim),
        floatingLabelStyle: body.copyWith(color: accent),
        helperStyle: type.bodySmall!.copyWith(color: textDim),
        errorStyle: type.bodySmall!.copyWith(color: toneFaultInk),
        focusColor: accent,
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(foregroundColor: text)
            .merge(_action(focusRing))
            .merge(upstream.textButtonTheme.style),
      ),
      elevatedButtonTheme: ElevatedButtonThemeData(
        style: ElevatedButton.styleFrom(
          backgroundColor: accent,
          foregroundColor: bg,
          disabledBackgroundColor: overlaySoftStrong,
          disabledForegroundColor: textFaint,
        )
            .merge(_action(const BorderSide(color: text, width: 2)))
            .merge(upstream.elevatedButtonTheme.style),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          backgroundColor: surfaceRaised,
          foregroundColor: text,
          disabledForegroundColor: textFaint,
        )
            .merge(_action(focusRing))
            .merge(upstream.outlinedButtonTheme.style),
      ),
      switchTheme: upstream.switchTheme.copyWith(
        thumbColor: _onOff(accent, textDim),
        trackColor: _onOff(overlayAccent, overlaySoftStrong),
        trackOutlineColor: const WidgetStatePropertyAll(Colors.transparent),
      ),
      checkboxTheme: upstream.checkboxTheme.copyWith(
        fillColor: _onOff(accent, Colors.transparent),
        checkColor: const WidgetStatePropertyAll(bg),
        side: BorderSide(color: textDim, width: 1.5),
        shape: square,
      ),
      radioTheme: upstream.radioTheme.copyWith(fillColor: _onOff(accent, textDim)),
      tabBarTheme: upstream.tabBarTheme.copyWith(
        labelColor: text,
        unselectedLabelColor: textDim,
        indicatorColor: accent,
        indicator: const UnderlineTabIndicator(
            borderSide: BorderSide(color: accent, width: 2)),
      ),
      listTileTheme: upstream.listTileTheme.copyWith(
        iconColor: text,
        textColor: text,
        selectedColor: accent,
        selectedTileColor: surfaceRaised,
        shape: square,
      ),
      popupMenuTheme: upstream.popupMenuTheme.copyWith(
        color: surface,
        shape: square.copyWith(side: BorderSide(color: border)),
      ),
      menuBarTheme: MenuBarThemeData(
          style: MenuStyle(
              backgroundColor: const WidgetStatePropertyAll(surface),
              shape: const WidgetStatePropertyAll(square))),
      menuTheme: MenuThemeData(
          style: MenuStyle(
              backgroundColor: const WidgetStatePropertyAll(surface),
              shape: WidgetStatePropertyAll(
                  square.copyWith(side: BorderSide(color: border))))),
      tooltipTheme: upstream.tooltipTheme.copyWith(
        decoration: BoxDecoration(color: surfaceRaised),
        textStyle: body.copyWith(color: text),
      ),
      snackBarTheme: SnackBarThemeData(
        backgroundColor: surfaceRaised,
        contentTextStyle: body.copyWith(color: text),
        actionTextColor: accent,
        shape: square,
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

  static WidgetStateProperty<Color?> _onOff(Color on, Color off) =>
      WidgetStateProperty.resolveWith((states) =>
          states.contains(WidgetState.selected) ? on : off);
}
