package com.ethosprotocol

import androidx.compose.ui.test.*
import androidx.compose.ui.test.junit4.createComposeRule
import com.ethosprotocol.models.Vault
import com.ethosprotocol.models.VaultStatus
import com.ethosprotocol.ui.screens.VaultListScreen
import dagger.hilt.android.testing.HiltAndroidRule
import dagger.hilt.android.testing.HiltAndroidTest
import org.junit.Before
import org.junit.Rule
import org.junit.Test

@HiltAndroidTest
class VaultListScreenTest {

    @get:Rule(order = 0) val hiltRule = HiltAndroidRule(this)
    @get:Rule(order = 1) val composeRule = createComposeRule()

    @Before fun setup() { hiltRule.inject() }

    @Test
    fun emptyState_showsCreatePrompt() {
        composeRule.setContent { VaultListScreen(onVaultClick = {}) }
        composeRule.onNodeWithText("No vaults yet", substring = true).assertIsDisplayed()
    }

    @Test
    fun addButton_isDisplayed() {
        composeRule.setContent { VaultListScreen(onVaultClick = {}) }
        composeRule.onNodeWithContentDescription("Create vault").assertIsDisplayed()
    }
}
