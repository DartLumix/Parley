import 'package:flutter_test/flutter_test.dart';
import 'package:parley/src/report.dart';

void main() {
  group('report issue', () {
    test('the title uses the first description line', () {
      final url = githubIssueUrl(
        'Audio disappears\nwhile Minecraft is open',
        'OS: Linux',
        '/tmp/report',
      );
      expect(url.host, 'github.com');
      expect(url.path, '/DartLumix/Parley/issues/new');
      expect(url.queryParameters['title'], '[Bug] Audio disappears');
      expect(url.queryParameters['body'], contains('while Minecraft is open'));
    });

    test('the body asks for the saved report files', () {
      final url = githubIssueUrl('No sound', 'OS: Linux', '/home/a b/report');
      final body = url.queryParameters['body']!;
      expect(body, contains('/home/a b/report'));
      expect(body, contains('session-current.log'));
      expect(body, contains('screenshot.png'));
    });

    test('an empty description still produces a usable issue', () {
      final url = githubIssueUrl('', 'OS: Windows', r'C:\report');
      expect(url.queryParameters['title'], '[Bug] Problem report');
      expect(url.queryParameters['body'], contains('Please describe'));
    });
  });
}
