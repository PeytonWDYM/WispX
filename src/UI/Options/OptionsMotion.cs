using System;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Animation;

namespace WispX
{
    /// <summary>Short, one-shot transitions shared by the Options window and pages.</summary>
    internal static class OptionsMotion
    {
        public static DoubleAnimation To(double from, double to, int ms, int delay = 0) => new(from, to, TimeSpan.FromMilliseconds(ms))
        { BeginTime = TimeSpan.FromMilliseconds(delay), EasingFunction = new CubicEase { EasingMode = EasingMode.EaseOut } };

        public static void Pop(FrameworkElement element, int delay = 0)
        {
            var scale = new ScaleTransform(1, 1);
            element.RenderTransformOrigin = new Point(0.5, 0.5);
            element.RenderTransform = scale;
            scale.BeginAnimation(ScaleTransform.ScaleXProperty, To(0.85, 1, 200, delay));
            scale.BeginAnimation(ScaleTransform.ScaleYProperty, To(0.85, 1, 200, delay));
            element.BeginAnimation(UIElement.OpacityProperty, To(0, 1, 200, delay));
        }
    }
}
