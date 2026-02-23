package com.example;

import static org.junit.Assert.*;
import org.junit.Test;

public class StringUtilsTest {

    private final StringUtils utils = new StringUtils();

    @Test
    public void testReverse() {
        assertEquals("cba", utils.reverse("abc"));
        assertNull(utils.reverse(null));
    }

    @Test
    public void testIsPalindrome() {
        assertTrue(utils.isPalindrome("racecar"));
        assertFalse(utils.isPalindrome("hello"));
        assertFalse(utils.isPalindrome(null));
    }

    @Test
    public void testCountVowels() {
        assertEquals(2, utils.countVowels("hello"));
        assertEquals(0, utils.countVowels(null));
        assertEquals(5, utils.countVowels("aeiou"));
    }

    // Intentionally NOT testing truncate to leave some uncovered lines
}
