global class Formatter {
    global static String format(String value) {
        return value == null ? '' : value.trim();
    }

    @TestVisible
    private static Boolean isBlank(String value) {
        return String.isBlank(value);
    }
}
