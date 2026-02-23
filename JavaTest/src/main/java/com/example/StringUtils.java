package com.example;

public class StringUtils {

    public String reverse(String s) {
        if (s == null) {
            return null;
        }
        return new StringBuilder(s).reverse().toString();
    }

    public boolean isPalindrome(String s) {
        if (s == null) {
            return false;
        }
        String reversed = reverse(s);
        return s.equals(reversed);
    }

    public int countVowels(String s) {
        if (s == null) {
            return 0;
        }
        int count = 0;
        for (char c : s.toLowerCase().toCharArray()) {
            if ("aeiou".indexOf(c) >= 0) {
                count++;
            }
        }
        return count;
    }

    public String truncate(String s, int maxLen) {
        if (s == null || s.length() <= maxLen) {
            return s;
        }
        return s.substring(0, maxLen) + "...";
    }
}
