using System.Windows;
using System.Windows.Controls;

namespace WispX
{
    /// <summary>Local Codex sign-in status; checking never makes a network request.</summary>
    public partial class AccountPage : UserControl
    {
        public AccountPage() { InitializeComponent(); Refresh(); }
        private void Refresh()
        {
            string email = ChatGptTranscriber.LoginEmail();
            bool signedIn = email != null;
            Status.Text = signedIn ? $"Signed in as {email}" : "Not signed in";
            Detail.Text = signedIn ? "Using your ChatGPT login from the Codex app" : "Open the Codex app and sign in";
            StatusIcon.Text = signedIn ? "\uE73E" : "\uE7BA";
            StatusIcon.SetResourceReference(TextBlock.ForegroundProperty, signedIn ? "SystemFillColorSuccessBrush" : "SystemFillColorCautionBrush");
        }
        private void CheckAgain(object sender, RoutedEventArgs e) { Refresh(); OptionsMotion.Pop(StatusIcon); }
    }
}
